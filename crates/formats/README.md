# `craft-anything-formats`

Pure, standalone, panic-free (`unsafe_code = "forbid"`, `#![deny(clippy::unwrap_used, ...)]`) file-format parsers for **Craft Anything**.

## Format Classification (`storytold/craft-libs#778`)

Formats in this crate fall into two categories:

1. **Already-shared formats (`craft-libs`)**: thin wrappers or re-exports over shared `craft-*` crates in `craft-libs`. Do not re-extract or duplicate these.
2. **Single-app inspector formats (Craft Anything only)**: proprietary or domain-specific asset, package, and binary readers that have no external `storytold` consumer. Keep these in `craft-anything-formats` unless a concrete shared consumer in another `storytold` repository is identified.

### Already-Shared Formats (`craft-libs`)

| Module(s) | Shared Crate | Notes |
| --- | --- | --- |
| `dex.rs`, `dex_ops.rs` | `craft-dex` | Dalvik DEX & opcodes |
| `elf.rs` | `craft-elf` | ELF executables & shared objects |
| `evm.rs` | `craft-evm` | EVM bytecode |
| `har.rs` | `craft-har` | HTTP Archive (HAR) |
| `java_class.rs`, `java_ops.rs` | `craft-java-class` | JVM `.class` files & opcodes |
| `jsonc.rs` | `craft-jsonc` | JSON with comments |
| `macho.rs` | `craft-macho` | Mach-O thin & universal binaries |
| `pcap.rs` | `craft-pcap` | PCAP / PCAPNG packet captures |
| `pe.rs` | `craft-pe` | PE/COFF executables & DLLs |
| `pkcs7.rs` | `craft-pkcs7` | PKCS#7 / CMS SignedData |
| `plist.rs` | `craft-plist` | XML & binary property lists |
| `sourcemap.rs` | `craft-sourcemap` | JavaScript source maps |
| `sqlite.rs` | `craft-sqlite` | SQLite database inspector |
| `tarcpio.rs` | `craft-tar` | TAR & CPIO archive walkers |
| `wasm.rs` | `craft-wasm` | WebAssembly modules & components |
| `zip.rs`, `zip_file.rs` | `craft-zip` | Lenient ZIP & seekable entry reader |

(`*_legacy.rs` and `*_legacy_tests.rs` files alongside the wrappers above are retained solely for parity/regression tests against the `craft-libs` cutover.)

### Single-App Inspector Formats (Craft Anything Only)

Verified across `storytold` repositories: **ASAR** (`asar.rs`) and **Android AXML/ARSC** (`axml.rs`, `arsc.rs`) have no consumers outside `storytold/craft-anything`.

- **Desktop / JS / Scripting App Packages**
  - `asar.rs` — Electron ASAR archive reader & test builder
  - `bun_program.rs` — `bun build --compile` embedded module graph
  - `node_sea.rs` — Node.js Single Executable Application (`NODE_SEA_BLOB`)
  - `pyc.rs` — Python `.pyc` bytecode & marshal headers
  - `pyinstaller.rs` — PyInstaller `MEI` / CArchive TOC
  - `dart.rs` — Flutter / Dart AOT snapshot & ELF dynamic symbol reader
- **Android-Specific Formats** (beyond `craft-dex` / `craft-zip` / `craft-pkcs7`)
  - `axml.rs` — Android binary XML (`AndroidManifest.xml`, `res/*.xml`), string pool, manifest facts
  - `arsc.rs` — Android compiled resource tables (`resources.arsc`)
  - `apk_sign.rs` — APK Signing Block (v2/v3/v3.1) signer & digest reader
- **Apple / macOS / iOS-Specific Formats** (beyond `craft-macho` / `craft-plist`)
  - `car.rs` — Compiled asset catalogs (`Assets.car` / BOM store)
  - `codesign.rs` — Mach-O `LC_CODE_SIGNATURE` superblobs, code directories, entitlements
  - `keyed_archive.rs` — `NSKeyedArchiver` object-graph decoder
  - `nib.rs` — Compiled Interface Builder `.nib` / `NIBArchive`
  - `macho_deps.rs` — Mach-O load-command dependency & rpath helpers
  - `swift.rs`, `swift_demangle.rs` — Swift symbol classification & demangling
- **Game Engine Asset & Package Readers**
  - `godot_pck.rs` — Godot `.pck` resource packs
  - `il2cpp.rs` — Unity IL2CPP `global-metadata.dat`
  - `unity_audio.rs`, `unity_bundle.rs`, `unity_serialized.rs`, `unity_texture.rs` — UnityFS bundles, `.assets`, Texture2D, AudioClip/FSB5
  - `unreal_iostore.rs`, `unreal_pak.rs`, `unreal_texture.rs` — Unreal `.utoc`/`.ucas`, `.pak`, `.uasset` cooked textures
  - `bptc.rs` — BPTC (BC6H/BC7) block texture decoder
- **Windows / .NET / Native / Firmware / Other**
  - `authenticode.rs` — PE Authenticode certificate table & PKCS#7 inspection
  - `dos.rs` — DOS MZ/COM header reader
  - `dotnet/`, `dotnet_bundle.rs` — .NET CLI metadata tables & single-file apphost bundles
  - `firmware.rs` — Firmware images (SquashFS, U-Boot, FDT, Android boot, EFI)
  - `go.rs` — Go pclntab, buildinfo, and type metadata
  - `rust_bin.rs` — Rust binary metadata & panic/version hints
  - `solana.rs` — Solana BPF/SBF ELF metadata & IDL hints
  - `ai_model.rs` — GGUF & safetensors header-only model inspector
  - `packed.rs`, `packed_carrier.rs` — Packer / carrier detection
  - `verify.rs` — Pure-Rust RSA/ECDSA signature verification
- **Internal Helpers & Glue**
  - `binary.rs`, `bytes.rs`, `common.rs`, `decompress.rs`, `der.rs`, `fsread.rs` (`fsread/`), `hints.rs`, `pixels.rs` (`pixels/`), `strings.rs`
