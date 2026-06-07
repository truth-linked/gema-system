#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/tcp.h>
#include <unistd.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef void *CUstream;
typedef uint64_t CUdeviceptr;

#define CUDA_SUCCESS 0
#define CUDA_ERROR_INVALID_VALUE 1
#define CUDA_ERROR_NOT_INITIALIZED 3
#define CUDA_ERROR_NOT_SUPPORTED 801
#define CUDA_ERROR_UNKNOWN 999

static int g_initialized = 0;
static unsigned long g_request_id = 1;
static int g_proxy_fd = -1;
static int g_cached_driver = 0;
static int g_cached_count = -1;
static int g_cached_device0 = 0;
static char g_cached_name0[256] = {0};
static size_t g_cached_mem0 = 0;
static int g_cached_major0 = 0;
static int g_cached_minor0 = 0;
static int g_launch_already_synced = 0;

static const char *endpoint(void) {
    const char *value = getenv("GEMA_CUDA_PROXY_ENDPOINT");
    return value && *value ? value : "127.0.0.1:47619";
}

static int parse_endpoint(const char *value, char *host, size_t host_len, char *port, size_t port_len) {
    const char *prefix = "tcp://";
    if (strncmp(value, prefix, strlen(prefix)) == 0) {
        value += strlen(prefix);
    }
    const char *colon = strrchr(value, ':');
    if (!colon || colon == value || !colon[1]) {
        return -1;
    }
    size_t n = (size_t)(colon - value);
    if (n >= host_len) {
        return -1;
    }
    memcpy(host, value, n);
    host[n] = 0;
    snprintf(port, port_len, "%s", colon + 1);
    return 0;
}

static int connect_proxy(void) {
    char host[256], port[32];
    if (parse_endpoint(endpoint(), host, sizeof(host), port, sizeof(port)) != 0) {
        return -1;
    }
    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_family = AF_UNSPEC;
    struct addrinfo *res = NULL;
    if (getaddrinfo(host, port, &hints, &res) != 0) {
        return -1;
    }
    int fd = -1;
    for (struct addrinfo *it = res; it; it = it->ai_next) {
        fd = socket(it->ai_family, it->ai_socktype, it->ai_protocol);
        if (fd < 0) {
            continue;
        }
        struct timeval tv;
        tv.tv_sec = 5;
        tv.tv_usec = 0;
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
        if (connect(fd, it->ai_addr, it->ai_addrlen) == 0) {
            break;
        }
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    return fd;
}

static void close_proxy(void) {
    if (g_proxy_fd >= 0) {
        close(g_proxy_fd);
        g_proxy_fd = -1;
    }
}

static int proxy_fd(void) {
    if (g_proxy_fd >= 0) {
        return g_proxy_fd;
    }
    g_proxy_fd = connect_proxy();
    return g_proxy_fd;
}

__attribute__((destructor)) static void proxy_destructor(void) {
    close_proxy();
}

static int json_int(const char *json, const char *key, long long *out) {
    char needle[128];
    snprintf(needle, sizeof(needle), "\"%s\":", key);
    const char *p = strstr(json, needle);
    if (!p) {
        return -1;
    }
    p += strlen(needle);
    char *end = NULL;
    long long value = strtoll(p, &end, 10);
    if (end == p) {
        return -1;
    }
    *out = value;
    return 0;
}

static int json_string(const char *json, const char *key, char *out, size_t out_len) {
    char needle[128];
    snprintf(needle, sizeof(needle), "\"%s\":\"", key);
    const char *p = strstr(json, needle);
    if (!p) {
        return -1;
    }
    p += strlen(needle);
    size_t i = 0;
    while (*p && *p != '"' && i + 1 < out_len) {
        if (*p == '\\' && p[1]) {
            p++;
        }
        out[i++] = *p++;
    }
    out[i] = 0;
    return 0;
}

static int json_find_device0(const char *json, const char *key, long long *out) {
    const char *devices = strstr(json, "\"devices\":[");
    if (!devices) return -1;
    return json_int(devices, key, out);
}

static int json_find_device0_string(const char *json, const char *key, char *out, size_t out_len) {
    const char *devices = strstr(json, "\"devices\":[");
    if (!devices) return -1;
    return json_string(devices, key, out, out_len);
}

static CUresult proxy_call(const char *method, const char *params, char *response, size_t response_len) {
    int fd = proxy_fd();
    if (fd < 0) {
        return CUDA_ERROR_NOT_INITIALIZED;
    }
    size_t request_len = strlen(method) + strlen(params ? params : "{}") + 128;
    char *request = (char *)malloc(request_len);
    if (!request) {
        close(fd);
        return CUDA_ERROR_UNKNOWN;
    }
    unsigned long id = g_request_id++;
    snprintf(request, request_len, "{\"id\":%lu,\"method\":\"%s\",\"params\":%s}\n", id, method, params ? params : "{}");
    size_t total = strlen(request);
    if (write(fd, request, total) != (ssize_t)total) {
        free(request);
        close_proxy();
        return CUDA_ERROR_UNKNOWN;
    }
    free(request);
    size_t pos = 0;
    while (pos + 1 < response_len) {
        char c;
        ssize_t n = read(fd, &c, 1);
        if (n <= 0) {
            close_proxy();
            break;
        }
        if (c == '\n') {
            break;
        }
        response[pos++] = c;
    }
    response[pos] = 0;
    long long code = CUDA_ERROR_UNKNOWN;
    if (json_int(response, "code", &code) == 0) {
        if (code != CUDA_SUCCESS && getenv("GEMA_CUDA_PROXY_TRACE")) {
            fprintf(stderr, "gema-cuda-proxy: %s -> %s\n", method, response);
        }
        return (CUresult)code;
    }
    if (!strstr(response, "\"ok\":true") && getenv("GEMA_CUDA_PROXY_TRACE")) {
        fprintf(stderr, "gema-cuda-proxy: %s -> %s\n", method, response);
    }
    return strstr(response, "\"ok\":true") ? CUDA_SUCCESS : CUDA_ERROR_UNKNOWN;
}

static char hex_digit(unsigned int value) {
    return (char)(value < 10 ? '0' + value : 'a' + (value - 10));
}

static char *hex_encode(const unsigned char *data, size_t len) {
    char *out = (char *)malloc(len * 2 + 1);
    if (!out) {
        return NULL;
    }
    for (size_t i = 0; i < len; i++) {
        out[i * 2] = hex_digit((data[i] >> 4) & 0xF);
        out[i * 2 + 1] = hex_digit(data[i] & 0xF);
    }
    out[len * 2] = 0;
    return out;
}

static char *json_escape_string(const char *value) {
    size_t len = strlen(value);
    char *out = (char *)malloc(len * 2 + 1);
    if (!out) {
        return NULL;
    }
    size_t pos = 0;
    for (size_t i = 0; i < len; i++) {
        unsigned char c = (unsigned char)value[i];
        if (c == '"' || c == '\\') {
            out[pos++] = '\\';
            out[pos++] = (char)c;
        } else if (c == '\n') {
            out[pos++] = '\\';
            out[pos++] = 'n';
        } else if (c == '\r') {
            out[pos++] = '\\';
            out[pos++] = 'r';
        } else if (c == '\t') {
            out[pos++] = '\\';
            out[pos++] = 't';
        } else {
            out[pos++] = (char)c;
        }
    }
    out[pos] = 0;
    return out;
}

static int hex_value(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static int json_hex_bytes(const char *json, const char *key, unsigned char *out, size_t out_len) {
    char needle[128];
    snprintf(needle, sizeof(needle), "\"%s\":\"", key);
    const char *p = strstr(json, needle);
    if (!p) {
        return -1;
    }
    p += strlen(needle);
    for (size_t i = 0; i < out_len; i++) {
        int hi = hex_value(p[i * 2]);
        int lo = hex_value(p[i * 2 + 1]);
        if (hi < 0 || lo < 0) {
            return -1;
        }
        out[i] = (unsigned char)((hi << 4) | lo);
    }
    return 0;
}

CUresult cuInit(unsigned int Flags) {
    char params[64], response[2048];
    snprintf(params, sizeof(params), "{\"flags\":%u}", Flags);
    CUresult code = proxy_call("bootstrap", params, response, sizeof(response));
    if (code == CUDA_SUCCESS) {
        g_initialized = 1;
        long long value = 0;
        if (json_int(response, "driver", &value) == 0) g_cached_driver = (int)value;
        if (json_int(response, "count", &value) == 0) g_cached_count = (int)value;
        if (json_find_device0(response, "device", &value) == 0) g_cached_device0 = (int)value;
        if (json_find_device0(response, "memory", &value) == 0) g_cached_mem0 = (size_t)value;
        if (json_find_device0(response, "major", &value) == 0) g_cached_major0 = (int)value;
        if (json_find_device0(response, "minor", &value) == 0) g_cached_minor0 = (int)value;
        json_find_device0_string(response, "name", g_cached_name0, sizeof(g_cached_name0));
    }
    return code;
}

CUresult cuDriverGetVersion(int *driverVersion) {
    if (!driverVersion) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (g_cached_driver > 0) {
        *driverVersion = g_cached_driver;
        return CUDA_SUCCESS;
    }
    char response[2048];
    CUresult code = proxy_call("cuDriverGetVersion", "{}", response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *driverVersion = (int)value;
    }
    return code;
}

CUresult cuDeviceGetCount(int *count) {
    if (!count) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (!g_initialized) {
        cuInit(0);
    }
    if (g_cached_count >= 0) {
        *count = g_cached_count;
        return CUDA_SUCCESS;
    }
    char response[2048];
    CUresult code = proxy_call("cuDeviceGetCount", "{}", response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *count = (int)value;
    }
    return code;
}

CUresult cuDeviceGet(CUdevice *device, int ordinal) {
    if (!device) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (ordinal == 0 && g_initialized) {
        *device = g_cached_device0;
        return CUDA_SUCCESS;
    }
    char params[64], response[2048];
    snprintf(params, sizeof(params), "{\"ordinal\":%d}", ordinal);
    CUresult code = proxy_call("cuDeviceGet", params, response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *device = (CUdevice)value;
    }
    return code;
}

CUresult cuDeviceGetName(char *name, int len, CUdevice dev) {
    if (!name || len <= 0) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (dev == g_cached_device0 && g_cached_name0[0]) {
        snprintf(name, (size_t)len, "%s", g_cached_name0);
        return CUDA_SUCCESS;
    }
    char params[128], response[4096], value[4096];
    snprintf(params, sizeof(params), "{\"device\":%d,\"size\":%d}", dev, len);
    CUresult code = proxy_call("cuDeviceGetName", params, response, sizeof(response));
    if (code == CUDA_SUCCESS && json_string(response, "result", value, sizeof(value)) == 0) {
        snprintf(name, (size_t)len, "%s", value);
    }
    return code;
}

CUresult cuDeviceTotalMem_v2(size_t *bytes, CUdevice dev) {
    if (!bytes) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (dev == g_cached_device0 && g_cached_mem0 > 0) {
        *bytes = g_cached_mem0;
        return CUDA_SUCCESS;
    }
    char params[64], response[2048];
    snprintf(params, sizeof(params), "{\"device\":%d}", dev);
    CUresult code = proxy_call("cuDeviceTotalMem_v2", params, response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *bytes = (size_t)value;
    }
    return code;
}

CUresult cuDeviceTotalMem(size_t *bytes, CUdevice dev) {
    return cuDeviceTotalMem_v2(bytes, dev);
}

CUresult cuDeviceComputeCapability(int *major, int *minor, CUdevice dev) {
    if (!major || !minor) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (dev == g_cached_device0 && g_cached_major0 > 0) {
        *major = g_cached_major0;
        *minor = g_cached_minor0;
        return CUDA_SUCCESS;
    }
    char params[64], response[2048];
    snprintf(params, sizeof(params), "{\"device\":%d}", dev);
    CUresult code = proxy_call("cuDeviceComputeCapability", params, response, sizeof(response));
    long long maj = 0, min = 0;
    if (code == CUDA_SUCCESS) {
        if (json_int(response, "major", &maj) == 0) {
            *major = (int)maj;
        }
        if (json_int(response, "minor", &min) == 0) {
            *minor = (int)min;
        }
    }
    return code;
}

CUresult cuDeviceGetAttribute(int *pi, int attrib, CUdevice dev) {
    if (!pi) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"device\":%d,\"attribute\":%d}", dev, attrib);
    CUresult code = proxy_call("cuDeviceGetAttribute", params, response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *pi = (int)value;
    }
    return code;
}

CUresult cuGetErrorName(CUresult error, const char **pStr) {
    if (!pStr) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    switch (error) {
    case CUDA_SUCCESS:
        *pStr = "CUDA_SUCCESS";
        break;
    case CUDA_ERROR_INVALID_VALUE:
        *pStr = "CUDA_ERROR_INVALID_VALUE";
        break;
    case CUDA_ERROR_NOT_INITIALIZED:
        *pStr = "CUDA_ERROR_NOT_INITIALIZED";
        break;
    case CUDA_ERROR_NOT_SUPPORTED:
        *pStr = "CUDA_ERROR_NOT_SUPPORTED";
        break;
    default:
        *pStr = "CUDA_ERROR_UNKNOWN";
        break;
    }
    return CUDA_SUCCESS;
}

CUresult cuGetErrorString(CUresult error, const char **pStr) {
    return cuGetErrorName(error, pStr);
}

CUresult cuCtxCreate_v2(CUcontext *pctx, unsigned int flags, CUdevice dev) {
    if (!pctx) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"flags\":%u,\"device\":%d}", flags, dev);
    CUresult code = proxy_call("cuCtxCreate_v2", params, response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *pctx = (CUcontext)(uintptr_t)value;
    }
    return code;
}

CUresult cuCtxDestroy_v2(CUcontext ctx) {
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"ctx\":%llu}", (unsigned long long)(uintptr_t)ctx);
    return proxy_call("cuCtxDestroy_v2", params, response, sizeof(response));
}

CUresult cuCtxSynchronize(void) {
    if (g_launch_already_synced) {
        g_launch_already_synced = 0;
        return CUDA_SUCCESS;
    }
    char response[2048];
    return proxy_call("cuCtxSynchronize", "{}", response, sizeof(response));
}

CUresult cuMemAlloc_v2(CUdeviceptr *dptr, size_t bytesize) {
    if (!dptr || bytesize == 0) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"bytesize\":%zu}", bytesize);
    CUresult code = proxy_call("cuMemAlloc_v2", params, response, sizeof(response));
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *dptr = (CUdeviceptr)value;
    }
    return code;
}

CUresult cuMemFree_v2(CUdeviceptr dptr) {
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"dptr\":%llu}", (unsigned long long)dptr);
    return proxy_call("cuMemFree_v2", params, response, sizeof(response));
}

CUresult cuMemcpyHtoD_v2(CUdeviceptr dstDevice, const void *srcHost, size_t ByteCount) {
    if (!srcHost && ByteCount > 0) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char *hex = hex_encode((const unsigned char *)srcHost, ByteCount);
    if (!hex) {
        return CUDA_ERROR_UNKNOWN;
    }
    size_t params_len = strlen(hex) + 256;
    char *params = (char *)malloc(params_len);
    if (!params) {
        free(hex);
        return CUDA_ERROR_UNKNOWN;
    }
    snprintf(params, params_len, "{\"dst\":%llu,\"bytesize\":%zu,\"data\":\"%s\"}", (unsigned long long)dstDevice, ByteCount, hex);
    free(hex);
    char response[2048];
    CUresult code = proxy_call("cuMemcpyHtoD_v2", params, response, sizeof(response));
    free(params);
    return code;
}

CUresult cuMemcpyDtoH_v2(void *dstHost, CUdeviceptr srcDevice, size_t ByteCount) {
    if (!dstHost && ByteCount > 0) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char params[128];
    snprintf(params, sizeof(params), "{\"src\":%llu,\"bytesize\":%zu}", (unsigned long long)srcDevice, ByteCount);
    size_t response_len = ByteCount * 2 + 2048;
    char *response = (char *)malloc(response_len);
    if (!response) {
        return CUDA_ERROR_UNKNOWN;
    }
    CUresult code = proxy_call("cuMemcpyDtoH_v2", params, response, response_len);
    if (code == CUDA_SUCCESS && json_hex_bytes(response, "data", (unsigned char *)dstHost, ByteCount) != 0) {
        code = CUDA_ERROR_UNKNOWN;
    }
    free(response);
    return code;
}

CUresult cuMemsetD8_v2(CUdeviceptr dstDevice, unsigned char uc, size_t N) {
    char params[160], response[2048];
    snprintf(params, sizeof(params), "{\"dst\":%llu,\"value\":%u,\"count\":%zu}", (unsigned long long)dstDevice, (unsigned int)uc, N);
    return proxy_call("cuMemsetD8_v2", params, response, sizeof(response));
}

CUresult cuModuleLoadData(CUmodule *module, const void *image) {
    if (!module || !image) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char *escaped = json_escape_string((const char *)image);
    if (!escaped) {
        return CUDA_ERROR_UNKNOWN;
    }
    size_t params_len = strlen(escaped) + 64;
    char *params = (char *)malloc(params_len);
    if (!params) {
        free(escaped);
        return CUDA_ERROR_UNKNOWN;
    }
    snprintf(params, params_len, "{\"image\":\"%s\"}", escaped);
    free(escaped);
    char response[2048];
    CUresult code = proxy_call("cuModuleLoadData", params, response, sizeof(response));
    free(params);
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *module = (CUmodule)(uintptr_t)value;
    }
    return code;
}

CUresult cuModuleGetFunction(CUfunction *hfunc, CUmodule hmod, const char *name) {
    if (!hfunc || !hmod || !name) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char *escaped = json_escape_string(name);
    if (!escaped) {
        return CUDA_ERROR_UNKNOWN;
    }
    size_t params_len = strlen(escaped) + 128;
    char *params = (char *)malloc(params_len);
    if (!params) {
        free(escaped);
        return CUDA_ERROR_UNKNOWN;
    }
    snprintf(params, params_len, "{\"module\":%llu,\"name\":\"%s\"}", (unsigned long long)(uintptr_t)hmod, escaped);
    free(escaped);
    char response[2048];
    CUresult code = proxy_call("cuModuleGetFunction", params, response, sizeof(response));
    free(params);
    long long value = 0;
    if (code == CUDA_SUCCESS && json_int(response, "result", &value) == 0) {
        *hfunc = (CUfunction)(uintptr_t)value;
    }
    return code;
}

CUresult cuModuleUnload(CUmodule hmod) {
    char params[128], response[2048];
    snprintf(params, sizeof(params), "{\"module\":%llu}", (unsigned long long)(uintptr_t)hmod);
    return proxy_call("cuModuleUnload", params, response, sizeof(response));
}

CUresult cuLaunchKernel(CUfunction f, unsigned int gridDimX, unsigned int gridDimY, unsigned int gridDimZ,
                        unsigned int blockDimX, unsigned int blockDimY, unsigned int blockDimZ,
                        unsigned int sharedMemBytes, CUstream hStream, void **kernelParams, void **extra) {
    if (!f) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    if (kernelParams) {
        return CUDA_ERROR_NOT_SUPPORTED;
    }
    const void *param_buffer = NULL;
    size_t param_size = 0;
    if (extra) {
        for (size_t i = 0; extra[i] != NULL; i += 2) {
            if (extra[i] == (void *)1) {
                param_buffer = extra[i + 1];
            } else if (extra[i] == (void *)2) {
                param_size = *(size_t *)extra[i + 1];
            }
        }
    }
    if (!param_buffer && param_size > 0) {
        return CUDA_ERROR_INVALID_VALUE;
    }
    char *hex = hex_encode((const unsigned char *)param_buffer, param_size);
    if (!hex) {
        return CUDA_ERROR_UNKNOWN;
    }
    size_t params_len = strlen(hex) + 512;
    char *params = (char *)malloc(params_len);
    if (!params) {
        free(hex);
        return CUDA_ERROR_UNKNOWN;
    }
    snprintf(params, params_len,
             "{\"function\":%llu,\"grid_x\":%u,\"grid_y\":%u,\"grid_z\":%u,\"block_x\":%u,\"block_y\":%u,\"block_z\":%u,\"shared_mem\":%u,\"stream\":%llu,\"param_buffer\":\"%s\"}",
             (unsigned long long)(uintptr_t)f, gridDimX, gridDimY, gridDimZ, blockDimX, blockDimY, blockDimZ,
             sharedMemBytes, (unsigned long long)(uintptr_t)hStream, hex);
    if (getenv("GEMA_CUDA_PROXY_TRACE")) {
        fprintf(stderr, "gema-cuda-proxy: cuLaunchKernel params %s\n", params);
    }
    free(hex);
    char response[2048];
    CUresult code = proxy_call("cuLaunchKernel", params, response, sizeof(response));
    free(params);
    if (code == CUDA_SUCCESS) {
        g_launch_already_synced = 1;
    }
    return code;
}
