#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fixture_dir="${repo_root}/tests/fixtures/video"
cache_dir="${repo_root}/tests/fixtures/.cache"
tool_dir="${repo_root}/target/vigil-test-tools/mediamtx/v1.19.1"
zig_tool_dir="${repo_root}/target/vigil-test-tools/zig"

person_sha="a65415f0da868f59014777ace1b702f6d7c6274c18e5af3e344cf710c37526ea"
person_clip="${fixture_dir}/one-by-one-person-detection.mp4"
empty_clip="${fixture_dir}/empty-scene-from-one-by-one-person-detection.mp4"
empty_sha="2d4c35233e497d1c81d2a08187e856b5aba84acaf4f10cb47ccd33b9b5edee63"
model_dir="${repo_root}/tests/fixtures/models"
model_sha="9de513de589ac98bb92d3bca53b5af7b9acfa9b0bacb831f7999d0f7afaee8f0"
model_file="${model_dir}/yolox-tiny-coco.pth"

mediamtx_version="v1.19.1"
mediamtx_asset=""
mediamtx_sha=""
zig_version="0.13.0"
zig_asset=""
zig_sha=""

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    mediamtx_asset="mediamtx_${mediamtx_version}_linux_amd64.tar.gz"
    mediamtx_sha="035ee04f91b1c7a0c02e13b2139ca2456e43b6bd6a80e3100e8c228556e07807"
    ;;
  Linux-aarch64|Linux-arm64)
    mediamtx_asset="mediamtx_${mediamtx_version}_linux_arm64.tar.gz"
    mediamtx_sha="97a277cf24153e168008c18da53fe84e8d364456e2d7b457dc0457666c32867b"
    ;;
  Linux-armv7l)
    mediamtx_asset="mediamtx_${mediamtx_version}_linux_armv7.tar.gz"
    mediamtx_sha="052654f2268ad0604f2bb277e417cf3c122d7399f814e6d1ca2dbcf180ed7fe9"
    ;;
  *)
    echo "Unsupported platform for pinned mediamtx download: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    zig_asset="zig-linux-x86_64-${zig_version}.tar.xz"
    zig_sha="d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea"
    ;;
  Linux-aarch64|Linux-arm64)
    zig_asset="zig-linux-aarch64-${zig_version}.tar.xz"
    zig_sha="041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556"
    ;;
  *)
    echo "Unsupported platform for pinned Zig download: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

need_tool() {
  local name="$1"
  if ! command -v "${name}" >/dev/null 2>&1; then
    echo "${name} is required. Install ${name} or put it on PATH before running this setup." >&2
    exit 1
  fi
}

need_tool curl
need_tool sha256sum
need_tool tar
need_tool ffmpeg
need_tool ffprobe

write_zig_wrapper() {
  local wrapper="$1"
  local target_triple="$2"
  local compiler="$3"
  cat >"${wrapper}" <<EOF
#!/usr/bin/env bash
set -euo pipefail
zig_bin=${zig_bin@Q}
target_triple=${target_triple@Q}
compiler=${compiler@Q}
args=()
for arg in "\$@"; do
  case "\${arg}" in
    --target=*) ;;
    *) args+=("\${arg}") ;;
  esac
done
exec "\${zig_bin}" "\${compiler}" -target "\${target_triple}" "\${args[@]}"
EOF
  chmod +x "${wrapper}"
}

zig_archive_dir() {
  local target_triple="$1"
  local archive_name="$2"
  local probe_cpp="${zig_tool_dir}/probe-${target_triple}.cpp"
  local probe_obj="${zig_tool_dir}/probe-${target_triple}.o"
  local trace
  local archive

  printf 'extern "C" int zig_probe(void) { return 0; }\n' >"${probe_cpp}"
  "${zig_bin}" c++ -target "${target_triple}" -c "${probe_cpp}" -o "${probe_obj}"
  trace="$("${zig_bin}" cc -target "${target_triple}" -### "${probe_obj}" -lc++ -o "${probe_obj}.out" 2>&1 || true)"
  archive="$(printf '%s\n' "${trace}" | grep -o "/[^\" ]*/${archive_name}" | head -n 1 || true)"
  if [[ -z "${archive}" ]]; then
    echo "Unable to locate Zig ${archive_name} for ${target_triple}" >&2
    exit 1
  fi
  dirname "${archive}"
}

write_rust_lld_wrapper() {
  local wrapper="$1"
  local libcxx_dir="$2"
  local libcxxabi_dir="$3"
  local rust_lld

  rust_lld="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | awk '/host:/ { print $2 }')/bin/rust-lld"
  if [[ ! -x "${rust_lld}" ]]; then
    echo "Unable to locate rust-lld at ${rust_lld}" >&2
    exit 1
  fi

  cat >"${wrapper}" <<EOF
#!/usr/bin/env bash
set -euo pipefail
rust_lld=${rust_lld@Q}
libcxx_dir=${libcxx_dir@Q}
libcxxabi_dir=${libcxxabi_dir@Q}
args=("-L" "\${libcxx_dir}" "-L" "\${libcxxabi_dir}")
skip_next=false
for arg in "\$@"; do
  if [[ "\${skip_next}" == true ]]; then
    skip_next=false
    continue
  fi
  if [[ "\${arg}" == "-flavor" ]]; then
    skip_next=true
    continue
  fi
  args+=("\${arg}")
  if [[ "\${arg}" == "-lc++" ]]; then
    args+=("-lc++abi")
  fi
done
exec "\${rust_lld}" -flavor gnu "\${args[@]}"
EOF
  chmod +x "${wrapper}"
}

mkdir -p "${cache_dir}" "${tool_dir}" "${zig_tool_dir}"

echo "${person_sha}  ${person_clip}" | sha256sum --check
echo "${empty_sha}  ${empty_clip}" | sha256sum --check
echo "${model_sha}  ${model_file}" | sha256sum --check
ffprobe -hide_banner -loglevel error -select_streams v:0 -count_frames \
  -show_entries stream=nb_read_frames,duration -of default=noprint_wrappers=1 "${person_clip}" >/dev/null
ffprobe -hide_banner -loglevel error -select_streams v:0 -count_frames \
  -show_entries stream=nb_read_frames,duration -of default=noprint_wrappers=1 "${empty_clip}" >/dev/null

mediamtx_archive="${cache_dir}/${mediamtx_asset}"
mediamtx_url="https://github.com/bluenviron/mediamtx/releases/download/${mediamtx_version}/${mediamtx_asset}"
if [[ ! -f "${mediamtx_archive}" ]]; then
  curl -L --fail --output "${mediamtx_archive}" "${mediamtx_url}"
fi
echo "${mediamtx_sha}  ${mediamtx_archive}" | sha256sum --check
tar -xzf "${mediamtx_archive}" -C "${tool_dir}" mediamtx
chmod +x "${tool_dir}/mediamtx"

zig_archive="${cache_dir}/${zig_asset}"
zig_url="https://ziglang.org/download/${zig_version}/${zig_asset}"
zig_dir="${zig_tool_dir}/${zig_asset%.tar.xz}"
if [[ ! -f "${zig_archive}" ]]; then
  curl -L --fail --output "${zig_archive}" "${zig_url}"
fi
echo "${zig_sha}  ${zig_archive}" | sha256sum --check
if [[ ! -x "${zig_dir}/zig" ]]; then
  tar -xf "${zig_archive}" -C "${zig_tool_dir}"
fi
zig_bin="${zig_dir}/zig"
write_zig_wrapper "${zig_tool_dir}/zig-cc-x86_64-linux-musl" "x86_64-linux-musl" "cc"
write_zig_wrapper "${zig_tool_dir}/zig-cxx-x86_64-linux-musl" "x86_64-linux-musl" "c++"
write_zig_wrapper "${zig_tool_dir}/zig-cc-aarch64-linux-musl" "aarch64-linux-musl" "cc"
write_zig_wrapper "${zig_tool_dir}/zig-cxx-aarch64-linux-musl" "aarch64-linux-musl" "c++"

x86_libcxx_dir="$(zig_archive_dir "x86_64-linux-musl" "libc++.a")"
x86_libcxxabi_dir="$(zig_archive_dir "x86_64-linux-musl" "libc++abi.a")"
aarch64_libcxx_dir="$(zig_archive_dir "aarch64-linux-musl" "libc++.a")"
aarch64_libcxxabi_dir="$(zig_archive_dir "aarch64-linux-musl" "libc++abi.a")"
write_rust_lld_wrapper "${zig_tool_dir}/rust-lld-x86_64-linux-musl" "${x86_libcxx_dir}" "${x86_libcxxabi_dir}"
write_rust_lld_wrapper "${zig_tool_dir}/rust-lld-aarch64-linux-musl" "${aarch64_libcxx_dir}" "${aarch64_libcxxabi_dir}"

echo "ffmpeg: $(command -v ffmpeg)"
echo "ffprobe: $(command -v ffprobe)"
echo "mediamtx: ${tool_dir}/mediamtx"
echo "zig: ${zig_bin}"
echo "zig cc x86_64 musl: ${zig_tool_dir}/zig-cc-x86_64-linux-musl"
echo "zig c++ x86_64 musl: ${zig_tool_dir}/zig-cxx-x86_64-linux-musl"
echo "rust-lld x86_64 musl: ${zig_tool_dir}/rust-lld-x86_64-linux-musl"
echo "zig cc aarch64 musl: ${zig_tool_dir}/zig-cc-aarch64-linux-musl"
echo "zig c++ aarch64 musl: ${zig_tool_dir}/zig-cxx-aarch64-linux-musl"
echo "rust-lld aarch64 musl: ${zig_tool_dir}/rust-lld-aarch64-linux-musl"
echo "person fixture: ${person_clip}"
echo "empty fixture: ${empty_clip}"
echo "model fixture: ${model_file}"
