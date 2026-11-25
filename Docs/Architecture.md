# Architecture

```
 your files (outside the repository)        Leonida
 ───────────────────────────────────        ─────────────────────────────────────────
 game data ───────────────────────────────► Conversion + Title
                                            (Formats, Shader)
                                                       │
                                                       ▼
                                            cache directory
                                                       │
 game executable ─► Loader ───────────────► title code (native x86-64)
                    prepare, map, bind         │ imports
                                               ▼
                                            Services ── host Windows
                                               │ D3D12.X
                                               ▼
                                            Graphics ─► host D3D12
                                               │ shaders at pipeline creation
                                               ▼
                                            Shader ◄─► cache directory
```

Shaders are translated when the title creates a pipeline, from the bytes it passes. Translating them ahead of time with `LeonidaWarm.exe` only warms the cache.

## Import resolution

Every `dll!function` the title imports is resolved in this order:

1. the implementation in `leonida-services` (always wins);
2. the host function, when the host policy allows that DLL;
3. an export of a DLL shipped beside the executable, mapped like the executable;
4. a generated stub that names the import when it is called.

`LoadLibrary` of a console DLL that Leonida implements returns a pseudo handle, and `GetProcAddress` on it finds Leonida's functions. Title DLLs are mapped on demand. `QueryApiImpl` hands out runtime objects by provider and interface id; online areas answer as an offline console.

Title storage is a console drive. The file services rewrite paths on a virtual drive to the host folder behind it; persistent local storage lives under `LEONIDA_DATA_DIR`.

An executable reaches its static TLS as slot 0. Mapped as a DLL it gets another slot, so the loader redirects those accesses. `Leonida run --execute` installs a crash reporter that names faults as `module+rva` and unwinds the faulting thread.

## Shaders

The shader crate turns a native GFX10 program into SSA, one instruction per machine instruction. Structurisation removes EXEC: lane masks become per-lane booleans and wave-wide mask tests become per-lane branches. Resource tracing follows every descriptor and scalar memory address back to the root parameter it comes from, including waterfall loops that read a descriptor from the first active lane. The result is emitted as DXIL and validated before it reaches the driver.
