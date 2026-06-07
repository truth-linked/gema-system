#!/usr/bin/env python3
import argparse
import ctypes
import json
import socketserver
import socket
import threading


CUDA_SUCCESS = 0
CUDA_ERROR_INVALID_VALUE = 1
CUDA_ERROR_NOT_INITIALIZED = 3
CUDA_ERROR_NOT_SUPPORTED = 801


class CudaBackend:
    def __init__(self, fake=False):
        self.fake = fake
        self._lock = threading.Lock()
        self._cuda = None
        self._next_handle = 1
        self._fake_memory = {}
        self._fake_context = 1
        self._fake_modules = {}
        self._fake_functions = {}
        if not fake:
            self._cuda = ctypes.CDLL("libcuda.so.1")
            self._configure_cuda_abi()

    def _configure_cuda_abi(self):
        self._cuda.cuInit.argtypes = [ctypes.c_uint]
        self._cuda.cuInit.restype = ctypes.c_int
        self._cuda.cuDriverGetVersion.argtypes = [ctypes.POINTER(ctypes.c_int)]
        self._cuda.cuDriverGetVersion.restype = ctypes.c_int
        self._cuda.cuDeviceGetCount.argtypes = [ctypes.POINTER(ctypes.c_int)]
        self._cuda.cuDeviceGetCount.restype = ctypes.c_int
        self._cuda.cuDeviceGet.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_int]
        self._cuda.cuDeviceGet.restype = ctypes.c_int
        self._cuda.cuDeviceGetName.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        self._cuda.cuDeviceGetName.restype = ctypes.c_int
        total_mem = getattr(self._cuda, "cuDeviceTotalMem_v2", None) or self._cuda.cuDeviceTotalMem
        total_mem.argtypes = [ctypes.POINTER(ctypes.c_size_t), ctypes.c_int]
        total_mem.restype = ctypes.c_int
        self._cuda.cuDeviceComputeCapability.argtypes = [
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
            ctypes.c_int,
        ]
        self._cuda.cuDeviceComputeCapability.restype = ctypes.c_int
        self._cuda.cuDeviceGetAttribute.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_int, ctypes.c_int]
        self._cuda.cuDeviceGetAttribute.restype = ctypes.c_int
        self._cuda.cuCtxCreate_v2.argtypes = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint, ctypes.c_int]
        self._cuda.cuCtxCreate_v2.restype = ctypes.c_int
        self._cuda.cuCtxDestroy_v2.argtypes = [ctypes.c_void_p]
        self._cuda.cuCtxDestroy_v2.restype = ctypes.c_int
        self._cuda.cuCtxSynchronize.argtypes = []
        self._cuda.cuCtxSynchronize.restype = ctypes.c_int
        self._cuda.cuMemAlloc_v2.argtypes = [ctypes.POINTER(ctypes.c_uint64), ctypes.c_size_t]
        self._cuda.cuMemAlloc_v2.restype = ctypes.c_int
        self._cuda.cuMemFree_v2.argtypes = [ctypes.c_uint64]
        self._cuda.cuMemFree_v2.restype = ctypes.c_int
        self._cuda.cuMemcpyHtoD_v2.argtypes = [ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t]
        self._cuda.cuMemcpyHtoD_v2.restype = ctypes.c_int
        self._cuda.cuMemcpyDtoH_v2.argtypes = [ctypes.c_void_p, ctypes.c_uint64, ctypes.c_size_t]
        self._cuda.cuMemcpyDtoH_v2.restype = ctypes.c_int
        self._cuda.cuMemsetD8_v2.argtypes = [ctypes.c_uint64, ctypes.c_uint, ctypes.c_size_t]
        self._cuda.cuMemsetD8_v2.restype = ctypes.c_int
        self._cuda.cuModuleLoadData.argtypes = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p]
        self._cuda.cuModuleLoadData.restype = ctypes.c_int
        self._cuda.cuModuleGetFunction.argtypes = [
            ctypes.POINTER(ctypes.c_void_p),
            ctypes.c_void_p,
            ctypes.c_char_p,
        ]
        self._cuda.cuModuleGetFunction.restype = ctypes.c_int
        self._cuda.cuModuleUnload.argtypes = [ctypes.c_void_p]
        self._cuda.cuModuleUnload.restype = ctypes.c_int
        self._cuda.cuLaunchKernel.argtypes = [
            ctypes.c_void_p,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_uint,
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_void_p,
        ]
        self._cuda.cuLaunchKernel.restype = ctypes.c_int

    def call(self, method, params):
        try:
            if method == "ping":
                return {"ok": True, "result": {"backend": "fake" if self.fake else "libcuda"}}
            if method == "bootstrap":
                return self._bootstrap(params)
            if self.fake:
                return self._fake_call(method, params)
            with self._lock:
                return self._cuda_call(method, params)
        except Exception as exc:
            return {"ok": False, "error": str(exc), "code": CUDA_ERROR_NOT_SUPPORTED}

    def _bootstrap(self, params):
        init = self._fake_call("cuInit", params) if self.fake else self._cuda_call("cuInit", params)
        if init.get("code") != CUDA_SUCCESS:
            return init
        driver = self._fake_call("cuDriverGetVersion", {}) if self.fake else self._cuda_call("cuDriverGetVersion", {})
        count = self._fake_call("cuDeviceGetCount", {}) if self.fake else self._cuda_call("cuDeviceGetCount", {})
        devices = []
        for ordinal in range(int(count.get("result", 0))):
            dev = self._fake_call("cuDeviceGet", {"ordinal": ordinal}) if self.fake else self._cuda_call("cuDeviceGet", {"ordinal": ordinal})
            if dev.get("code") != CUDA_SUCCESS:
                continue
            handle = int(dev.get("result", ordinal))
            name = self._fake_call("cuDeviceGetName", {"device": handle}) if self.fake else self._cuda_call("cuDeviceGetName", {"device": handle})
            mem = self._fake_call("cuDeviceTotalMem_v2", {"device": handle}) if self.fake else self._cuda_call("cuDeviceTotalMem_v2", {"device": handle})
            cap = self._fake_call("cuDeviceComputeCapability", {"device": handle}) if self.fake else self._cuda_call("cuDeviceComputeCapability", {"device": handle})
            devices.append({
                "ordinal": ordinal,
                "device": handle,
                "name": name.get("result", ""),
                "memory": int(mem.get("result", 0)),
                "major": int(cap.get("major", 0)),
                "minor": int(cap.get("minor", 0)),
            })
        return {
            "ok": True,
            "code": CUDA_SUCCESS,
            "driver": int(driver.get("result", 0)),
            "count": int(count.get("result", 0)),
            "devices": devices,
        }

    def _fake_call(self, method, params):
        if method == "cuInit":
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuDriverGetVersion":
            return {"ok": True, "code": CUDA_SUCCESS, "result": 12040}
        if method == "cuDeviceGetCount":
            return {"ok": True, "code": CUDA_SUCCESS, "result": 1}
        if method == "cuDeviceGet":
            ordinal = int(params.get("ordinal", 0))
            return {"ok": True, "code": CUDA_SUCCESS, "result": ordinal}
        if method == "cuDeviceGetName":
            return {"ok": True, "code": CUDA_SUCCESS, "result": "Gema Remote CUDA Fake GPU"}
        if method == "cuDeviceTotalMem_v2":
            return {"ok": True, "code": CUDA_SUCCESS, "result": 24 * 1024 * 1024 * 1024}
        if method == "cuDeviceComputeCapability":
            return {"ok": True, "code": CUDA_SUCCESS, "major": 8, "minor": 6}
        if method == "cuDeviceGetAttribute":
            return {"ok": True, "code": CUDA_SUCCESS, "result": 0}
        if method == "cuCtxCreate_v2":
            return {"ok": True, "code": CUDA_SUCCESS, "result": self._fake_context}
        if method in ("cuCtxDestroy_v2", "cuCtxSynchronize"):
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuMemAlloc_v2":
            size = int(params.get("bytesize", 0))
            if size <= 0:
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "bytesize must be positive"}
            handle = self._next_handle
            self._next_handle += 1
            self._fake_memory[handle] = bytearray(size)
            return {"ok": True, "code": CUDA_SUCCESS, "result": handle}
        if method == "cuMemFree_v2":
            self._fake_memory.pop(int(params.get("dptr", 0)), None)
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuMemcpyHtoD_v2":
            dptr = int(params.get("dst", 0))
            data = bytes.fromhex(params.get("data", ""))
            size = int(params.get("bytesize", len(data)))
            buf = self._fake_memory.get(dptr)
            if buf is None or size > len(buf):
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "invalid destination"}
            buf[:size] = data[:size]
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuMemcpyDtoH_v2":
            sptr = int(params.get("src", 0))
            size = int(params.get("bytesize", 0))
            buf = self._fake_memory.get(sptr)
            if buf is None or size > len(buf):
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "invalid source"}
            return {"ok": True, "code": CUDA_SUCCESS, "data": bytes(buf[:size]).hex()}
        if method == "cuMemsetD8_v2":
            dptr = int(params.get("dst", 0))
            value = int(params.get("value", 0)) & 0xFF
            count = int(params.get("count", 0))
            buf = self._fake_memory.get(dptr)
            if buf is None or count > len(buf):
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "invalid destination"}
            buf[:count] = bytes([value]) * count
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuModuleLoadData":
            handle = self._next_handle
            self._next_handle += 1
            self._fake_modules[handle] = params.get("image", "")
            return {"ok": True, "code": CUDA_SUCCESS, "result": handle}
        if method == "cuModuleGetFunction":
            handle = self._next_handle
            self._next_handle += 1
            self._fake_functions[handle] = params.get("name", "")
            return {"ok": True, "code": CUDA_SUCCESS, "result": handle}
        if method == "cuModuleUnload":
            self._fake_modules.pop(int(params.get("module", 0)), None)
            return {"ok": True, "code": CUDA_SUCCESS}
        if method == "cuLaunchKernel":
            # Fake backend only supports the smoke kernel contract:
            # first 8 bytes = remote pointer, next 4 bytes = u32 count.
            data = bytes.fromhex(params.get("param_buffer", ""))
            if len(data) < 12:
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "param buffer too small"}
            dptr = int.from_bytes(data[:8], "little")
            count = int.from_bytes(data[8:12], "little")
            buf = self._fake_memory.get(dptr)
            if buf is None or count * 4 > len(buf):
                return {"ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": "invalid kernel destination"}
            pattern = (0x12345678).to_bytes(4, "little")
            for idx in range(count):
                buf[idx * 4 : idx * 4 + 4] = pattern
            return {"ok": True, "code": CUDA_SUCCESS}
        return {"ok": False, "code": CUDA_ERROR_NOT_SUPPORTED, "error": f"unsupported method: {method}"}

    def _cuda_call(self, method, params):
        if method == "cuInit":
            flags = ctypes.c_uint(int(params.get("flags", 0)))
            code = self._cuda.cuInit(flags)
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuDriverGetVersion":
            value = ctypes.c_int()
            code = self._cuda.cuDriverGetVersion(ctypes.byref(value))
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(value.value)}
        if method == "cuDeviceGetCount":
            value = ctypes.c_int()
            code = self._cuda.cuDeviceGetCount(ctypes.byref(value))
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(value.value)}
        if method == "cuDeviceGet":
            value = ctypes.c_int()
            ordinal = ctypes.c_int(int(params.get("ordinal", 0)))
            code = self._cuda.cuDeviceGet(ctypes.byref(value), ordinal)
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(value.value)}
        if method == "cuDeviceGetName":
            device = ctypes.c_int(int(params.get("device", 0)))
            size = int(params.get("size", 256))
            buf = ctypes.create_string_buffer(max(1, min(size, 4096)))
            code = self._cuda.cuDeviceGetName(buf, ctypes.c_int(len(buf)), device)
            return {
                "ok": code == CUDA_SUCCESS,
                "code": int(code),
                "result": buf.value.decode(errors="replace"),
            }
        if method == "cuDeviceTotalMem_v2":
            device = ctypes.c_int(int(params.get("device", 0)))
            value = ctypes.c_size_t()
            fn = getattr(self._cuda, "cuDeviceTotalMem_v2", None) or self._cuda.cuDeviceTotalMem
            code = fn(ctypes.byref(value), device)
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(value.value)}
        if method == "cuDeviceComputeCapability":
            device = ctypes.c_int(int(params.get("device", 0)))
            major = ctypes.c_int()
            minor = ctypes.c_int()
            code = self._cuda.cuDeviceComputeCapability(
                ctypes.byref(major), ctypes.byref(minor), device
            )
            return {
                "ok": code == CUDA_SUCCESS,
                "code": int(code),
                "major": int(major.value),
                "minor": int(minor.value),
            }
        if method == "cuDeviceGetAttribute":
            device = ctypes.c_int(int(params.get("device", 0)))
            attr = ctypes.c_int(int(params.get("attribute", 0)))
            value = ctypes.c_int()
            code = self._cuda.cuDeviceGetAttribute(ctypes.byref(value), attr, device)
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(value.value)}
        if method == "cuCtxCreate_v2":
            ctx = ctypes.c_void_p()
            flags = ctypes.c_uint(int(params.get("flags", 0)))
            device = ctypes.c_int(int(params.get("device", 0)))
            code = self._cuda.cuCtxCreate_v2(ctypes.byref(ctx), flags, device)
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(ctx.value or 0)}
        if method == "cuCtxDestroy_v2":
            ctx = ctypes.c_void_p(int(params.get("ctx", 0)))
            code = self._cuda.cuCtxDestroy_v2(ctx)
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuCtxSynchronize":
            code = self._cuda.cuCtxSynchronize()
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuMemAlloc_v2":
            ptr = ctypes.c_uint64()
            size = ctypes.c_size_t(int(params.get("bytesize", 0)))
            code = self._cuda.cuMemAlloc_v2(ctypes.byref(ptr), size)
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(ptr.value)}
        if method == "cuMemFree_v2":
            code = self._cuda.cuMemFree_v2(ctypes.c_uint64(int(params.get("dptr", 0))))
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuMemcpyHtoD_v2":
            data = bytes.fromhex(params.get("data", ""))
            size = int(params.get("bytesize", len(data)))
            src = ctypes.create_string_buffer(data[:size], size)
            code = self._cuda.cuMemcpyHtoD_v2(
                ctypes.c_uint64(int(params.get("dst", 0))),
                ctypes.cast(src, ctypes.c_void_p),
                ctypes.c_size_t(size),
            )
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuMemcpyDtoH_v2":
            size = int(params.get("bytesize", 0))
            dst = ctypes.create_string_buffer(size)
            code = self._cuda.cuMemcpyDtoH_v2(
                ctypes.cast(dst, ctypes.c_void_p),
                ctypes.c_uint64(int(params.get("src", 0))),
                ctypes.c_size_t(size),
            )
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "data": bytes(dst.raw).hex()}
        if method == "cuMemsetD8_v2":
            code = self._cuda.cuMemsetD8_v2(
                ctypes.c_uint64(int(params.get("dst", 0))),
                ctypes.c_uint(int(params.get("value", 0)) & 0xFF),
                ctypes.c_size_t(int(params.get("count", 0))),
            )
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuModuleLoadData":
            module = ctypes.c_void_p()
            image = params.get("image", "").encode("utf-8")
            buf = ctypes.create_string_buffer(image + b"\0")
            code = self._cuda.cuModuleLoadData(ctypes.byref(module), ctypes.cast(buf, ctypes.c_void_p))
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(module.value or 0)}
        if method == "cuModuleGetFunction":
            function = ctypes.c_void_p()
            module = ctypes.c_void_p(int(params.get("module", 0)))
            name = params.get("name", "").encode("utf-8")
            code = self._cuda.cuModuleGetFunction(ctypes.byref(function), module, ctypes.c_char_p(name))
            return {"ok": code == CUDA_SUCCESS, "code": int(code), "result": int(function.value or 0)}
        if method == "cuModuleUnload":
            module = ctypes.c_void_p(int(params.get("module", 0)))
            code = self._cuda.cuModuleUnload(module)
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        if method == "cuLaunchKernel":
            function = ctypes.c_void_p(int(params.get("function", 0)))
            param_buffer = bytes.fromhex(params.get("param_buffer", ""))
            param_size = len(param_buffer)
            param = ctypes.create_string_buffer(param_buffer, param_size)
            param_ptr = ctypes.cast(param, ctypes.c_void_p)
            param_size_value = ctypes.c_size_t(param_size)
            extra = (ctypes.c_void_p * 5)(
                ctypes.c_void_p(1),
                param_ptr,
                ctypes.c_void_p(2),
                ctypes.cast(ctypes.byref(param_size_value), ctypes.c_void_p),
                ctypes.c_void_p(0),
            )
            code = self._cuda.cuLaunchKernel(
                function,
                ctypes.c_uint(int(params.get("grid_x", 1))),
                ctypes.c_uint(int(params.get("grid_y", 1))),
                ctypes.c_uint(int(params.get("grid_z", 1))),
                ctypes.c_uint(int(params.get("block_x", 1))),
                ctypes.c_uint(int(params.get("block_y", 1))),
                ctypes.c_uint(int(params.get("block_z", 1))),
                ctypes.c_uint(int(params.get("shared_mem", 0))),
                ctypes.c_void_p(int(params.get("stream", 0))),
                None,
                ctypes.cast(extra, ctypes.POINTER(ctypes.c_void_p)),
            )
            if code != CUDA_SUCCESS and len(param_buffer) >= 12:
                dptr = ctypes.c_uint64(int.from_bytes(param_buffer[:8], "little"))
                count = ctypes.c_uint32(int.from_bytes(param_buffer[8:12], "little"))
                kernel_params = (ctypes.c_void_p * 3)(
                    ctypes.cast(ctypes.byref(dptr), ctypes.c_void_p),
                    ctypes.cast(ctypes.byref(count), ctypes.c_void_p),
                    ctypes.c_void_p(0),
                )
                code = self._cuda.cuLaunchKernel(
                    function,
                    ctypes.c_uint(int(params.get("grid_x", 1))),
                    ctypes.c_uint(int(params.get("grid_y", 1))),
                    ctypes.c_uint(int(params.get("grid_z", 1))),
                    ctypes.c_uint(int(params.get("block_x", 1))),
                    ctypes.c_uint(int(params.get("block_y", 1))),
                    ctypes.c_uint(int(params.get("block_z", 1))),
                    ctypes.c_uint(int(params.get("shared_mem", 0))),
                    ctypes.c_void_p(int(params.get("stream", 0))),
                    ctypes.cast(kernel_params, ctypes.c_void_p),
                    None,
                )
            return {"ok": code == CUDA_SUCCESS, "code": int(code)}
        return {"ok": False, "code": CUDA_ERROR_NOT_SUPPORTED, "error": f"unsupported method: {method}"}


class Handler(socketserver.StreamRequestHandler):
    def setup(self):
        super().setup()
        try:
            self.request.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError:
            pass

    def handle(self):
        backend = self.server.backend
        for raw in self.rfile:
            try:
                request = json.loads(raw.decode("utf-8"))
                response = backend.call(request.get("method"), request.get("params") or {})
                response["id"] = request.get("id")
            except Exception as exc:
                response = {"id": None, "ok": False, "code": CUDA_ERROR_INVALID_VALUE, "error": str(exc)}
            self.wfile.write((json.dumps(response, separators=(",", ":")) + "\n").encode("utf-8"))
            self.wfile.flush()


class Server(socketserver.TCPServer):
    allow_reuse_address = True


def main():
    parser = argparse.ArgumentParser(prog="gema-cuda-proxy-daemon")
    parser.add_argument("--listen", default="0.0.0.0:47619")
    parser.add_argument("--fake", action="store_true")
    args = parser.parse_args()
    host, port = args.listen.rsplit(":", 1)
    server = Server((host, int(port)), Handler)
    server.backend = CudaBackend(fake=args.fake)
    server.serve_forever()


if __name__ == "__main__":
    main()
