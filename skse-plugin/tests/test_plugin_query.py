"""Test the actual SKSE Query export without initializing Skyrim or hardware."""
import argparse
import ctypes as C
import json
from pathlib import Path


class SkseInterface(C.Structure):
    _fields_ = [(name, C.c_uint32) for name in
                ('skseVersion', 'runtimeVersion', 'editorVersion', 'isEditor')] + [
        (name, C.c_void_p) for name in
        ('QueryInterface', 'GetPluginHandle', 'GetReleaseIndex', 'GetPluginInfo')]


class PluginInfo(C.Structure):
    _fields_ = [('infoVersion', C.c_uint32), ('name', C.c_char_p),
                ('version', C.c_uint32)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('dll', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    dll = C.CDLL(str(args.dll.resolve()))
    query = dll.SKSEPlugin_Query
    query.argtypes = [C.POINTER(SkseInterface), C.POINTER(PluginInfo)]
    query.restype = C.c_bool
    cases = [
        ('SKSEVR 2.0.12 live runtime tag', 0x010400F1, 0, True),
        ('editor rejected', 0x010400F1, 1, False),
        ('SE 1.5.97 rejected', 0x01050610, 0, False),
        ('AE 1.6.640 rejected', 0x01062800, 0, False),
        ('older VR revision rejected', 0x010400E1, 0, False),
        ('newer VR revision rejected', 0x01040101, 0, False),
        ('unrecognized runtime rejected', 0, 0, False),
    ]
    results = []
    for name, runtime, editor, expected in cases:
        skse = SkseInterface(0x020000C0, runtime, 0, editor)
        info = PluginInfo()
        actual = bool(query(C.byref(skse), C.byref(info)))
        results.append(dict(case=name, runtime=f'{runtime:08X}',
                            expected=expected, actual=actual,
                            passed=actual == expected,
                            plugin=info.name.decode() if info.name else None,
                            pluginVersion=info.version))
    info = PluginInfo()
    actual = bool(query(None, C.byref(info)))
    results.append(dict(case='null SKSE interface rejected', passed=not actual))
    report = dict(passed=all(case['passed'] for case in results),
                  dll=str(args.dll.resolve()), cases=results)
    if args.output:
        args.output.write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(report, indent=2))
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
