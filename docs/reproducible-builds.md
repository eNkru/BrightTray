# Reproducible builds and verifying a download

Every release on the [releases page](https://github.com/eNkru/BrightTray/releases) ships four
files:

| File | What it is |
|------|-----------|
| `BrightTray.exe` | the tray app; the hook DLL is embedded in it |
| `brighttray-helper.exe` | the CLI (also the pipe server `brighttray-helper serve` runs) |
| `brighttray_hook.dll` | the DLL injected into `ShellHost.exe`; the CLI's `serve`/`inject` load it from next to their own executable |
| `SHA256SUMS.txt` | the SHA-256 of the other three, in `sha256sum` format |

BrightTray injects code into a Windows system process, so "is this download what the source
says" is a fair question. This page answers what can be answered today and is explicit about
what cannot.

## Verify a download

Download the four files into one folder, then:

**Windows (PowerShell)**

```powershell
Get-Content SHA256SUMS.txt | ForEach-Object {
  $hash, $name = $_ -split '\s+', 2
  $actual = (Get-FileHash -Algorithm SHA256 -Path $name.Trim()).Hash.ToLower()
  if ($actual -eq $hash) { "OK   $name" } else { "FAIL $name" }
}
```

**Linux**

```bash
sha256sum -c SHA256SUMS.txt
```

**macOS**

```bash
sed 's/\r$//' SHA256SUMS.txt | shasum -a 256 -c -
```

The `sed` is not decoration: the release's `SHA256SUMS.txt` is written by PowerShell on Windows
and therefore has CRLF line endings, which `shasum` folds into the file name and then cannot
find. GNU `sha256sum` tolerates them, which is why the Linux line above needs no equivalent.

Three `OK` lines mean the files are the ones that release published. A `FAIL` means the download
was altered or truncated — do not run it; download again, and if it still fails, open an issue
with the file name and the hash you got.

This verifies the file against the release page. It does **not** prove the release page itself is
honest: that would need a signature or an independent rebuild, and BrightTray has neither yet
(see [Limits](#limits)).

## Rebuild it

A release is built by `.github/workflows/ci.yml` on `windows-latest`, from the tagged commit,
with whatever stable Rust toolchain that runner image ships — the workflow prints it and does not
pin it. To repeat it by hand on Windows, from a checkout of that tag:

```powershell
# 1. Same toolchain: the workflow prints it, and a rebuild with a different rustc
#    will not match byte-for-byte.
rustc -Vv
cargo -V

# 2. Same path remapping: do not leak the build machine's paths into the binary.
$cargoHome = Join-Path $env:USERPROFILE '.cargo'
$env:RUSTFLAGS = "--remap-path-prefix=$PWD=. --remap-path-prefix=$cargoHome=cargo"

# 3. Same profile: Cargo.toml sets opt-level="s", lto=true, strip=true.
cargo build --release --workspace

# 4. The three artefacts the release collects.
target\release\BrightTray.exe
target\release\brighttray-helper.exe
target\release\brighttray_hook.dll

# 5. Compare with the release's own line (lowercase hash, two spaces, name).
(Get-FileHash -Algorithm SHA256 target\release\BrightTray.exe).Hash.ToLower()
Select-String -Path SHA256SUMS.txt -Pattern BrightTray.exe
```

A matching hash means your toolchain, lock file and `RUSTFLAGS` produced the same bytes. If the
two differ, check those three before suspecting the release.

`BrightTray.exe` embeds `brighttray_hook.dll` (built by `app/build.rs`, which inherits
`RUSTFLAGS`), so the hook is part of the app binary as well as a separate file.

## What is reproducible, measured

All numbers below were measured on 2026-11-20, with `x86_64-pc-windows-gnu` on Linux — the CI
builds with MSVC on Windows, which is exactly why the "different toolchain" row matters. Each
section gives the command that produced it; the hashes for a local build are per-machine by
nature, and where that is the point it is said so.

### The published sums verify

```
$ gh release download v0.1.2 --repo eNkru/BrightTray --dir ours
$ cd ours && sha256sum -c SHA256SUMS.txt
brighttray_hook.dll: OK
brighttray-helper.exe: OK
BrightTray.exe: OK
```

Asset sizes: 235,520 / 424,960 / 1,428,992 bytes (`stat -c '%s %n' *`).

### Same source, same toolchain, twice

The build under test, repeated after `touch app/src/main.rs` so cargo relinks:

```bash
export RUSTFLAGS="--remap-path-prefix=$(pwd)=. --remap-path-prefix=$HOME/.cargo=cargo"
cargo build --release --workspace --target x86_64-pc-windows-gnu
sha256sum target/x86_64-pc-windows-gnu/release/BrightTray.exe
x86_64-w64-mingw32-objdump -p target/x86_64-pc-windows-gnu/release/BrightTray.exe | grep Time/Date
```

then the same again with `RUSTFLAGS="… -C link-arg=-Wl,--no-insert-timestamp"` appended. The
nested hook build in `app/build.rs` inherits `RUSTFLAGS`, so the flag reaches the embedded DLL
too.

| Build | `BrightTray.exe` sha256 | PE `Time/Date` |
|-------|------------------------|----------------|
| default release profile, build 1 | `f4f2a871…` (varies per build and machine) | the build time |
| default, rebuild | `7cea3bca…` — **different** | later |
| deterministic flag, build 1 | `c3d99fe8…` | 1970-01-01 12:00:00 |
| deterministic flag, rebuild | `c3d99fe8…` — **byte-identical** | 1970-01-01 12:00:00 |

The only non-determinism in that experiment was the PE header timestamp, and one linker flag
removes it. The MSVC linker, which CI uses, gets `/Brepro` from rustc, which serves the same
purpose — **but that is not verified here**, because this machine has no MSVC linker. Verifying
it means building twice on `windows-latest` and comparing; the CI job does not do that today.

### Different toolchain or dependency set: expect a different binary

| Change | Effect |
|--------|--------|
| MSVC (CI) vs GNU (the test above) | different size, different imports, different layout |
| another `rustc` version | different codegen; the `/rustc/<hash>/` prefix inside the binary changes |
| another `Cargo.lock` | different dependency code |
| another `RUSTFLAGS` | the remapped prefixes are embedded in panic messages and paths |

So a hash that does not match a rebuild is only meaningful if the toolchain and lock file match.
If you are rebuilding to check a release, start from the tag and the committed `Cargo.lock`.

### Upstream `v1.0.0` versus this repository

The upstream project's last release predates this fork's build hardening. Comparing **its
`BrightTray.exe`** with **the released `v0.1.2` binary** from this repository (not with a local
build — a local GNU build is a third artefact, 1,540,608 bytes here):

```bash
gh release download v1.0.0 --repo cestbon0309/BrightTray --dir up
gh release download v0.1.2 --repo eNkru/BrightTray --dir ours
stat -c '%s %n' ours/BrightTray.exe up/BrightTray.exe
x86_64-w64-mingw32-objdump -p ours/BrightTray.exe | grep Time/Date
x86_64-w64-mingw32-objdump -p up/BrightTray.exe   | grep Time/Date
diff <(x86_64-w64-mingw32-objdump -p ours/BrightTray.exe | grep 'DLL Name' | sort) \
     <(x86_64-w64-mingw32-objdump -p up/BrightTray.exe   | grep 'DLL Name' | sort)
x86_64-w64-mingw32-strings -a ours/BrightTray.exe | grep -cE 'C:\\Users|/home/'
x86_64-w64-mingw32-strings -a up/BrightTray.exe   | grep -cE 'C:\\Users|/home/'
```

| Fact | upstream `v1.0.0` | this repo, released `v0.1.2` |
|------|-------------------|------------------------------|
| size | 1,343,488 B | 1,428,992 B |
| PE `Time/Date` | 2026-11-13 20:40:07 | 2026-11-18 23:44:29 |
| imported DLLs | — | identical list (the `diff` above is empty) |
| strings matching `C:\Users` or `/home/` | **46** | **0** |

The leak checks use `objdump`/`strings` from binutils, which on Windows come with Git Bash or
MSYS2. They are the same tools the CI's GNU cross-build uses; a PowerShell-only equivalent has
not been tested here, so it is not presented as a recipe.

The 46 upstream hits are the builder's own profile path, for example
`C:\Users\Cestbon\.cargo\registry\src\index.crates.io-…\bytes-1.12.1\src\lib.rs`. The released
`v0.1.2` binary contains none of them: the `RUSTFLAGS` remapping maps the checkout to `.` and
the cargo home to `cargo`, and only `/rustc/<hash>/library\…` paths from the standard library
remain — those name the compiler, not the builder.

That is the one provenance property this project can currently demonstrate on its own
artefacts: the shipped binary does not disclose where it was built.

## Limits

- **No signature.** Nothing binds a binary to this repository's identity. The checksum protects
  against a corrupted download, not against a replaced release.
- **No attestation.** There is no SLSA-style provenance record; the only evidence of how a
  release was built is the workflow file plus the commit it ran on.
- **CI reproducibility is unverified.** The `/Brepro` reasoning above is a claim about the
  toolchain, not a measurement made here. Closing it: build twice on the same runner image and
  compare the two `BrightTray.exe` hashes; if they match, promote the statement from "expected"
  to "measured", and if they do not, the difference is the thing to fix.
- **A pinned runner image** would make that comparison meaningful across time. Today
  `windows-latest` moves under the workflow, so a rebuild a year later will use a different
  toolchain.
- **The hook DLL has no version resource**, so a byte comparison is still the only way for the
  app to tell two hook builds apart. That is tracked internally as the hook-DLL version-identity
  follow-up from audit item P1-07; it does not affect download verification, which hashes the
  whole file.
