#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)

check_sha256() {
    local expected=$1
    local path=$2
    local actual
    actual=$(sha256sum "$path" | awk '{print $1}')
    if [[ "$actual" != "$expected" ]]; then
        printf 'provenance checksum mismatch: %s\nexpected: %s\nactual:   %s\n' \
            "$path" "$expected" "$actual" >&2
        return 1
    fi
}

check_common_licenses() {
    local crate_dir=$1
    check_sha256 \
        c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 \
        "$crate_dir/LICENSES/Apache-2.0.txt"
    check_sha256 \
        3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986 \
        "$crate_dir/LICENSES/GPL-3.0-or-later.txt"
    check_sha256 \
        3c1e0660f782af0b4b31eac50fc1f4b3c890c0d04963fe4c9d94ccf90ca52c84 \
        "$crate_dir/LICENSES/MulanPSL-2.0.txt"
}

axsched="$repo_root/crates/thekernel-axsched"
check_sha256 \
    374d6e997e4cf9db00d57c89af6c9b8b6cd3c8f31af27b5d3b95f23ad9a0ca89 \
    "$axsched/Cargo.toml.orig"
check_sha256 \
    5290ef7506251dc28d096b8ffd5c8f6bb15fcd759d84b36002bc20d468c655c9 \
    "$axsched/.cargo_vcs_info.json"
check_common_licenses "$axsched"
grep -Fq 'cad6b7b0b8d9ad1d52a834d8b7721114413da8cf3430af928b1c8651f911287a' \
    "$axsched/VENDOR.md"
grep -Fq '4d86c55dce4c87dde52792515ce188081323ac07' "$axsched/VENDOR.md"

axpoll="$repo_root/crates/thekernel-axpoll"
check_sha256 \
    3debbfb6c8878ea36d06d7e026e04e2828c73e58969992e4a4422852fb21f019 \
    "$axpoll/Cargo.toml.orig"
check_sha256 \
    802046df863df79c6080e7ed8131889ebeb3f46c4ab33b02ea03e1150ceaa8f5 \
    "$axpoll/.cargo_vcs_info.json"
check_common_licenses "$axpoll"
grep -Fq '36b92f85c6903350f5146216ccb7d7a7e7b4dbd6f5927a1279db03ba52a53ae7' \
    "$axpoll/VENDOR.md"
grep -Fq '86f20f6bc1b470fc21894721e72b721f49aa20b7' "$axpoll/VENDOR.md"

axtask="$repo_root/crates/thekernel-axtask"
check_sha256 \
    6c89e66d9e8755e6a7d9dcbba97db45612f62f7df398e80759ec930bb6a744b9 \
    "$axtask/Cargo.toml.orig"
check_sha256 \
    3744692589490c1122d7943acc064c337a23d4f4bc1d0156f28d41eb7b67efe3 \
    "$axtask/.cargo_vcs_info.json"
check_common_licenses "$axtask"
grep -Fq 'bc45120776afddf28b19bb7aba87e379c5779cf28a8f7884943a4821caeec774' \
    "$axtask/VENDOR.md"
grep -Fq '6c6765c05df0550e31edb0ca82d468199f108b3f' "$axtask/VENDOR.md"
grep -Fq 'dbbaea9ff0ee6c63bdfb9d9828d4a8d25ba8d0b1' "$axtask/VENDOR.md"

axcbpf="$repo_root/crates/thekernel-axcbpf"
check_sha256 \
    c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 \
    "$axcbpf/LICENSES/Apache-2.0.txt"
grep -Fq 'license = "Apache-2.0"' "$axcbpf/Cargo.toml"

axfault="$repo_root/crates/thekernel-axfault"
check_sha256 \
    c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 \
    "$axfault/LICENSES/Apache-2.0.txt"
grep -Fq 'license = "Apache-2.0"' "$axfault/Cargo.toml"

axtlb="$repo_root/crates/thekernel-axtlb"
check_sha256 \
    c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4 \
    "$axtlb/LICENSES/Apache-2.0.txt"
grep -Fq 'license = "Apache-2.0"' "$axtlb/Cargo.toml"

printf 'provenance: PASS\n'
