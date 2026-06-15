#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fixture_dir="${repo_root}/tests/fixtures/video"
cache_dir="${repo_root}/tests/fixtures/.cache"
tool_dir="${repo_root}/target/vigil-test-tools/mediamtx/v1.19.1"

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

mkdir -p "${cache_dir}" "${tool_dir}"

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

echo "ffmpeg: $(command -v ffmpeg)"
echo "ffprobe: $(command -v ffprobe)"
echo "mediamtx: ${tool_dir}/mediamtx"
echo "person fixture: ${person_clip}"
echo "empty fixture: ${empty_clip}"
echo "model fixture: ${model_file}"
