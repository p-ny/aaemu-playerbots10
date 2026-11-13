import ctypes as c, os

CELL = 1024.0

class Pak:
    def __init__(self, root):
        os.add_dll_directory(str(root / 'Bin64'))
        d = c.WinDLL(str(root / 'Bin64' / 'xlpack.dll'))
        def fn(n, a, r):
            f = getattr(d, n); f.argtypes = a; f.restype = r; return f
        if not fn('?CreateFileSystem@@YA_NXZ', [], c.c_bool)():
            raise RuntimeError('xlpack file system unavailable')
        self.open = fn('?FOpen@@YAPEAUFile@@PEBD0@Z', [c.c_char_p, c.c_char_p], c.c_void_p)
        self.size = fn('?FSize@@YA_JPEAUFile@@@Z', [c.c_void_p], c.c_int64)
        self.read_ = fn('?FRead@@YA_JPEAUFile@@PEAD_J@Z', [c.c_void_p, c.c_void_p, c.c_int64], c.c_int64)
        self.close = fn('?FClose@@YAXAEAPEAUFile@@@Z', [c.POINTER(c.c_void_p)], None)
        mount = fn('?Mount@@YAPEAXPEBD0_N@Z', [c.c_char_p, c.c_char_p, c.c_bool], c.c_void_p)
        if not mount(b'/', str(root / 'game_pak').encode(), False):
            raise RuntimeError('cannot mount the client pak')

    def read(self, name):
        f = self.open(('game/' + name).encode(), b'rb')
        if not f:
            return None
        try:
            size = self.size(f); buf = c.create_string_buffer(size)
            if self.read_(f, buf, size) != size:
                raise RuntimeError('short pak read: ' + name)
            return buf.raw
        finally:
            h = c.c_void_p(f); self.close(c.byref(h))


