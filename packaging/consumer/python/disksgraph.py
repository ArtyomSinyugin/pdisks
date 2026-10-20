"""Python 3 stdlib client for the packaged libdisksgraph.so C JSON bridge.

The same library also exports Stabby. No Rust compiler or Python extension is
needed. Keep this owner alive and close all contexts before releasing callbacks.
"""

import ctypes
import json


# Native workers may outlive ordinary Python references; explicit close releases this owner.
_LIVE_CONTEXTS = {}


class Library:
    """Own a mapped library and host callbacks until their worker contexts close."""

    def __init__(self, path):
        """Load an explicitly trusted shared library and set every C signature."""
        self._library = ctypes.CDLL(str(path))
        self._callbacks = {}
        self._callback_type = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.c_uint64,
                                               ctypes.c_void_p, ctypes.c_size_t)
        self._version = self._library.disksgraph_json_api_version
        self._version.argtypes = []
        self._version.restype = ctypes.c_uint32
        if self._version() != 1:
            raise RuntimeError("unsupported disksgraph C JSON API")
        self._open = self._library.disksgraph_json_open
        self._open.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
        self._open.restype = ctypes.c_void_p
        self._call = self._library.disksgraph_json_call
        self._call.argtypes = [ctypes.c_uint64, ctypes.c_char_p, ctypes.c_size_t]
        self._call.restype = ctypes.c_void_p
        self._close = self._library.disksgraph_json_close
        self._close.argtypes = [ctypes.c_uint64]
        self._close.restype = ctypes.c_void_p
        self._free = self._library.disksgraph_json_free
        self._free.argtypes = [ctypes.c_void_p]
        self._free.restype = None
        self._open_policy = self._library.disksgraph_json_open_with_policy
        self._open_policy.argtypes = [ctypes.c_char_p, ctypes.c_size_t,
                                      self._callback_type, ctypes.c_uint64]
        self._open_policy.restype = ctypes.c_void_p

    def _reply(self, pointer):
        """Copy and free one library allocation exactly once, including error replies."""
        if not pointer:
            raise RuntimeError("disksgraph returned no JSON buffer")
        try:
            reply = json.loads(ctypes.string_at(pointer).decode("utf-8"))
        finally:
            self._free(pointer)
        if reply.get("protocol_version") != 1 or "error" in reply:
            raise RuntimeError(reply)
        return reply["result"]

    def open(self, root, manifest=None, policy=None, cookie=0):
        """Open saved/read mode or supply a synchronous host policy callable.

        Policy receives (cookie, versioned_request), returns exactly 1/0/-1 for
        satisfied/violated/unknown, and must not reenter this context. Exceptions
        return unknown. Authorization and scope evidence belong to this caller.
        """
        config = {"abi_version": 1, "protocol_version": 1, "root": str(root)}
        if manifest is not None:
            config["manifest"] = str(manifest)
        data = json.dumps(config).encode("utf-8")
        callback = None
        if policy is None:
            opened = self._reply(self._open(data, len(data)))
        else:
            def decide(value, pointer, length):
                """Keep Python exceptions from escaping a native owner-thread callback."""
                try:
                    request = json.loads(ctypes.string_at(pointer, length).decode("utf-8"))
                    if request.get("policy_version") != 1:
                        return -1
                    result = policy(value, request)
                    return result if type(result) is int and result in (-1, 0, 1) else -1
                except BaseException:
                    return -1
            callback = self._callback_type(decide)
            opened = self._reply(self._open_policy(data, len(data), callback, cookie))
        context = opened["context"]
        self._callbacks[context] = callback
        _LIVE_CONTEXTS[(id(self), context)] = self
        return context

    def call(self, context, method, **params):
        """Use the existing versioned application methods and instance-owned handles."""
        data = json.dumps({"protocol_version": 1,
                           "request": {"method": method, "params": params}}).encode("utf-8")
        return self._reply(self._call(context, data, len(data)))

    def close(self, context):
        """Join the worker before releasing its callback reference; failure retains it."""
        result = self._reply(self._close(context))
        self._callbacks.pop(context, None)
        _LIVE_CONTEXTS.pop((id(self), context), None)
        return result
