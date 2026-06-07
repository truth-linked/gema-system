#!/usr/bin/env bash
set -euo pipefail

SYSTEM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="${GEMA_CUDA_PROXY_BUILD:-/tmp/gema-cuda-proxy-build}"
PORT="${GEMA_CUDA_PROXY_TEST_PORT:-47629}"

"$SYSTEM_DIR/rootfs/cuda-proxy/build.sh" "$BUILD_DIR" >/dev/null
python3 "$BUILD_DIR/bin/gema-cuda-proxy-daemon" --fake --listen "127.0.0.1:$PORT" &
daemon_pid=$!
cleanup() {
  kill "$daemon_pid" >/dev/null 2>&1 || true
}
trap cleanup EXIT
sleep 1

cat > "$BUILD_DIR/smoke.c" <<'C'
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef unsigned long long CUdeviceptr;
typedef int (*cuInit_t)(unsigned int);
typedef int (*cuDriverGetVersion_t)(int *);
typedef int (*cuDeviceGetCount_t)(int *);
typedef int (*cuDeviceGet_t)(CUdevice *, int);
typedef int (*cuDeviceGetName_t)(char *, int, CUdevice);
typedef int (*cuDeviceTotalMem_v2_t)(size_t *, CUdevice);
typedef int (*cuCtxCreate_v2_t)(CUcontext *, unsigned int, CUdevice);
typedef int (*cuCtxDestroy_v2_t)(CUcontext);
typedef int (*cuCtxSynchronize_t)(void);
typedef int (*cuMemAlloc_v2_t)(CUdeviceptr *, size_t);
typedef int (*cuMemFree_v2_t)(CUdeviceptr);
typedef int (*cuMemcpyHtoD_v2_t)(CUdeviceptr, const void *, size_t);
typedef int (*cuMemcpyDtoH_v2_t)(void *, CUdeviceptr, size_t);
typedef int (*cuMemsetD8_v2_t)(CUdeviceptr, unsigned char, size_t);
typedef int (*cuModuleLoadData_t)(CUmodule *, const void *);
typedef int (*cuModuleGetFunction_t)(CUfunction *, CUmodule, const char *);
typedef int (*cuModuleUnload_t)(CUmodule);
typedef int (*cuLaunchKernel_t)(CUfunction, unsigned int, unsigned int, unsigned int, unsigned int, unsigned int, unsigned int, unsigned int, void *, void **, void **);

static const char *ptx =
".version 7.0\n"
".target sm_50\n"
".address_size 64\n"
".visible .entry fill_kernel(\n"
"  .param .u64 out,\n"
"  .param .u32 n\n"
") {\n"
"  .reg .pred %p;\n"
"  .reg .b32 %r<5>;\n"
"  .reg .b64 %rd<5>;\n"
"  ld.param.u64 %rd1, [out];\n"
"  ld.param.u32 %r1, [n];\n"
"  mov.u32 %r2, %tid.x;\n"
"  mov.u32 %r3, %ctaid.x;\n"
"  mov.u32 %r4, %ntid.x;\n"
"  mad.lo.u32 %r2, %r3, %r4, %r2;\n"
"  setp.ge.u32 %p, %r2, %r1;\n"
"  @%p bra DONE;\n"
"  mul.wide.u32 %rd2, %r2, 4;\n"
"  add.u64 %rd3, %rd1, %rd2;\n"
"  st.global.u32 [%rd3], 305419896;\n"
"DONE:\n"
"  ret;\n"
"}\n";

int main(void) {
  void *h = dlopen("libcuda.so.1", RTLD_NOW);
  if (!h) {
    fprintf(stderr, "dlopen failed: %s\n", dlerror());
    return 2;
  }
  cuInit_t cuInit = (cuInit_t)dlsym(h, "cuInit");
  cuDriverGetVersion_t cuDriverGetVersion = (cuDriverGetVersion_t)dlsym(h, "cuDriverGetVersion");
  cuDeviceGetCount_t cuDeviceGetCount = (cuDeviceGetCount_t)dlsym(h, "cuDeviceGetCount");
  cuDeviceGet_t cuDeviceGet = (cuDeviceGet_t)dlsym(h, "cuDeviceGet");
  cuDeviceGetName_t cuDeviceGetName = (cuDeviceGetName_t)dlsym(h, "cuDeviceGetName");
  cuDeviceTotalMem_v2_t cuDeviceTotalMem_v2 = (cuDeviceTotalMem_v2_t)dlsym(h, "cuDeviceTotalMem_v2");
  cuCtxCreate_v2_t cuCtxCreate_v2 = (cuCtxCreate_v2_t)dlsym(h, "cuCtxCreate_v2");
  cuCtxDestroy_v2_t cuCtxDestroy_v2 = (cuCtxDestroy_v2_t)dlsym(h, "cuCtxDestroy_v2");
  cuCtxSynchronize_t cuCtxSynchronize = (cuCtxSynchronize_t)dlsym(h, "cuCtxSynchronize");
  cuMemAlloc_v2_t cuMemAlloc_v2 = (cuMemAlloc_v2_t)dlsym(h, "cuMemAlloc_v2");
  cuMemFree_v2_t cuMemFree_v2 = (cuMemFree_v2_t)dlsym(h, "cuMemFree_v2");
  cuMemcpyHtoD_v2_t cuMemcpyHtoD_v2 = (cuMemcpyHtoD_v2_t)dlsym(h, "cuMemcpyHtoD_v2");
  cuMemcpyDtoH_v2_t cuMemcpyDtoH_v2 = (cuMemcpyDtoH_v2_t)dlsym(h, "cuMemcpyDtoH_v2");
  cuMemsetD8_v2_t cuMemsetD8_v2 = (cuMemsetD8_v2_t)dlsym(h, "cuMemsetD8_v2");
  cuModuleLoadData_t cuModuleLoadData = (cuModuleLoadData_t)dlsym(h, "cuModuleLoadData");
  cuModuleGetFunction_t cuModuleGetFunction = (cuModuleGetFunction_t)dlsym(h, "cuModuleGetFunction");
  cuModuleUnload_t cuModuleUnload = (cuModuleUnload_t)dlsym(h, "cuModuleUnload");
  cuLaunchKernel_t cuLaunchKernel = (cuLaunchKernel_t)dlsym(h, "cuLaunchKernel");
  if (!cuInit || !cuDriverGetVersion || !cuDeviceGetCount || !cuDeviceGet || !cuDeviceGetName || !cuDeviceTotalMem_v2 ||
      !cuCtxCreate_v2 || !cuCtxDestroy_v2 || !cuCtxSynchronize || !cuMemAlloc_v2 || !cuMemFree_v2 ||
      !cuMemcpyHtoD_v2 || !cuMemcpyDtoH_v2 || !cuMemsetD8_v2 || !cuModuleLoadData || !cuModuleGetFunction ||
      !cuModuleUnload || !cuLaunchKernel) {
    fprintf(stderr, "missing required symbol\n");
    return 3;
  }
  int version = 0, count = 0;
  CUdevice dev = -1;
  char name[256] = {0};
  size_t mem = 0;
  CUcontext ctx = 0;
  CUmodule mod = 0;
  CUfunction fn = 0;
  CUdeviceptr ptr = 0;
  unsigned char src[16], dst[16], zero[16];
  unsigned int kernel_out[16];
  for (int i = 0; i < 16; i++) { src[i] = (unsigned char)(i + 1); dst[i] = 0; zero[i] = 0xA5; }
  if (cuInit(0) != 0) return 4;
  if (cuDriverGetVersion(&version) != 0 || version != 12040) return 5;
  if (cuDeviceGetCount(&count) != 0 || count != 1) return 6;
  if (cuDeviceGet(&dev, 0) != 0 || dev != 0) return 7;
  if (cuDeviceGetName(name, sizeof(name), dev) != 0 || strstr(name, "Gema Remote CUDA") == NULL) return 8;
  if (cuDeviceTotalMem_v2(&mem, dev) != 0 || mem == 0) return 9;
  if (cuCtxCreate_v2(&ctx, 0, dev) != 0 || !ctx) return 10;
  if (cuMemAlloc_v2(&ptr, sizeof(kernel_out)) != 0 || !ptr) return 11;
  if (cuMemcpyHtoD_v2(ptr, src, sizeof(src)) != 0) return 12;
  if (cuMemcpyDtoH_v2(dst, ptr, sizeof(dst)) != 0 || memcmp(src, dst, sizeof(src)) != 0) return 13;
  if (cuMemsetD8_v2(ptr, 0, sizeof(src)) != 0) return 14;
  if (cuMemcpyDtoH_v2(zero, ptr, sizeof(zero)) != 0) return 15;
  for (int i = 0; i < 16; i++) if (zero[i] != 0) return 16;
  if (cuCtxSynchronize() != 0) return 17;
  if (cuModuleLoadData(&mod, ptx) != 0 || !mod) return 18;
  if (cuModuleGetFunction(&fn, mod, "fill_kernel") != 0 || !fn) return 19;
  struct { unsigned long long ptr; unsigned int n; } args = { ptr, 16 };
  void *extra[] = {(void *)1, &args, (void *)2, &(size_t){sizeof(args)}, 0};
  if (cuLaunchKernel(fn, 1, 1, 1, 16, 1, 1, 0, 0, 0, extra) != 0) return 20;
  if (cuCtxSynchronize() != 0) return 21;
  if (cuMemcpyDtoH_v2(kernel_out, ptr, sizeof(kernel_out)) != 0) return 22;
  for (int i = 0; i < 16; i++) if (kernel_out[i] != 0x12345678u) return 23;
  if (cuModuleUnload(mod) != 0) return 24;
  if (cuMemFree_v2(ptr) != 0) return 25;
  if (cuCtxDestroy_v2(ctx) != 0) return 26;
  printf("{\"ok\":true,\"driver\":%d,\"count\":%d,\"name\":\"%s\",\"memory\":%zu,\"memcpy_roundtrip\":true,\"memset_zero\":true,\"kernel_launch\":true}\n", version, count, name, mem);
  return 0;
}
C

gcc "$BUILD_DIR/smoke.c" -ldl -o "$BUILD_DIR/smoke"
GEMA_CUDA_PROXY_ENDPOINT="127.0.0.1:$PORT" LD_LIBRARY_PATH="$BUILD_DIR/lib" "$BUILD_DIR/smoke"
