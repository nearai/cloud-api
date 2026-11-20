#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

usage() {
    cat <<'HELP'
Usage: prepare-apt.sh --bundle ABS --snapshot YYYYMMDDTHHMMSSZ --config ABS
                      --debian-keyring ABS --status ABS
                      [--pins FILE] [--docker-keyring ABS]

Create an isolated, file-only APT configuration; print its absolute path.
Run APT separately with APT_CONFIG set to that path. Existing output paths
are refused. State is created beside the config as CONFIG.d. No packages
are installed and no network commands are run. Keyrings are explicit trust
inputs and their filenames must not contain whitespace. Input repositories,
keyrings and status must already be readable by the user running APT.
HELP
}
fail() { printf 'prepare-apt: %s\n' "$*" >&2; exit 1; }

# Check every component, including ancestors: realpath alone would hide links.
check_path() {
    local path=$1 part current='' tail
    [[ $path == /* && $path != / && ! $path =~ [[:cntrl:]] ]] || fail "unsafe absolute path: $path"
    tail=${path#/}
    [[ $tail != */ && $tail != *//* ]] || fail "noncanonical path: $path"
    local -a parts
    IFS=/ read -r -a parts <<<"$tail"
    for part in "${parts[@]}"; do
        [[ $part != . && $part != .. && -n $part ]] || fail "path traversal: $path"
        current+=/$part
        [[ ! -L $current ]] || fail "symlink is not allowed: $current"
    done
}
input_file() {
    check_path "$1"
    [[ -f $1 && -r $1 ]] || fail "input must be a readable regular file: $1"
}
key_file() {
    input_file "$1"
    [[ ! $1 =~ [[:space:]] ]] || fail "keyring filename contains whitespace: $1"
}
quote_config() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    printf '"%s"' "$value"
}
uri_path() {
    local value=$1 char i
    for ((i=0; i<${#value}; i++)); do
        char=${value:i:1}
        case $char in
            [a-zA-Z0-9/._~-]) printf '%s' "$char" ;;
            *) printf '%%%02X' "'$char" ;;
        esac
    done
}
valid_version() {
    local value=$1 upstream revision
    if [[ $value == *:* ]]; then
        [[ $value =~ ^[0-9]+: ]] || return 1
        value=${value#*:}
    fi
    [[ $value =~ ^[0-9][A-Za-z0-9.+:~\-]*$ ]] || return 1
    if [[ $value == *-* ]]; then
        upstream=${value%-*}; revision=${value##*-}
        [[ $upstream =~ ^[0-9][A-Za-z0-9.+:~\-]*$ && $revision =~ ^[A-Za-z0-9+.~]+$ ]] || return 1
    fi
}

bundle='' snapshot='' config='' debian_key='' status='' pins='' docker_key=''
declare -A seen=()
while (($#)); do
    case $1 in
        --help|-h) usage; exit 0 ;;
        --bundle|--snapshot|--config|--debian-keyring|--status|--pins|--docker-keyring)
            option=$1
            [[ -z ${seen[$option]+x} ]] || fail "duplicate option: $option"
            seen[$option]=1
            if (($# < 2)); then fail "missing value: $option"; fi
            [[ -n $2 && $2 != --* ]] || fail "missing value: $option"
            case $option in
                --bundle) bundle=$2 ;; --snapshot) snapshot=$2 ;; --config) config=$2 ;;
                --debian-keyring) debian_key=$2 ;; --status) status=$2 ;;
                --pins) pins=$2 ;; --docker-keyring) docker_key=$2 ;;
            esac
            shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
[[ -n $bundle && -n $snapshot && -n $config && -n $debian_key && -n $status ]] || { usage >&2; fail 'missing required argument'; }
[[ $snapshot =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || fail 'invalid snapshot timestamp'
check_path "$bundle"
check_path "$config"
key_file "$debian_key"
input_file "$status"
[[ -d $bundle && -r $bundle && -x $bundle ]] || fail 'bundle must be a readable directory'
[[ $config != "$bundle/"* ]] || fail 'config must be outside the input bundle'
[[ -d ${config%/*} ]] || fail 'config parent directory does not exist'
[[ ! -e $config && ! -L $config ]] || fail 'config output already exists'
scratch=$config.d
check_path "$scratch"
[[ ! -e $scratch && ! -L $scratch ]] || fail 'config state directory already exists'
[[ -z $(find "$bundle" -type l -print -quit) ]] || fail 'symlink within bundle'
for archive in "debian/$snapshot/dists/bookworm" "debian-security/$snapshot/dists/bookworm-security"; do
    [[ -d $bundle/debian/archive/$archive ]] || fail "missing snapshot repository: $archive"
done
if [[ -n $docker_key ]]; then
    key_file "$docker_key"
    [[ -d $bundle/docker/dists/bookworm ]] || fail 'missing Docker repository'
fi

# Parse and validate all pins before creating any output. No shell evaluation.
preferences=''
declare -A packages=()
if [[ -n $pins ]]; then
    [[ $pins == /* ]] || pins=$PWD/$pins
    input_file "$pins"
    while IFS= read -r line || [[ -n $line ]]; do
        [[ $line =~ ^[[:space:]]*$ || $line =~ ^[[:space:]]*# ]] && continue
        [[ $line =~ ^([a-z0-9][a-z0-9+.-]+(:amd64|:all)?)=([^[:space:]=]+)$ ]] || fail "invalid package=version pin: $line"
        package=${BASH_REMATCH[1]}; version=${BASH_REMATCH[3]}
        valid_version "$version" || fail "invalid Debian version: $version"
        [[ -z ${packages[$package]+x} ]] || fail "duplicate package pin: $package"
        packages[$package]=1
        printf -v stanza 'Package: %s\nPin: version %s\nPin-Priority: 1001\n\n' "$package" "$version"
        preferences+=$stanza
    done <"$pins"
fi

umask 022
created_scratch=false
created_config=false
complete=false
cleanup() {
    if [[ $complete == false ]]; then
        [[ $created_config == false ]] || rm -f -- "$config"
        [[ $created_scratch == false ]] || rm -rf -- "$scratch"
    fi
}
trap cleanup EXIT
mkdir -- "$scratch"
created_scratch=true
mkdir -p -- "$scratch/etc/apt.conf.d" "$scratch/etc/sources.list.d" \
    "$scratch/etc/auth.conf.d" "$scratch/etc/trusted.gpg.d" "$scratch/etc/preferences.d" \
    "$scratch/state/lists/partial" "$scratch/cache/archives/partial" "$scratch/log"
for empty in apt.conf sources.list auth.conf trusted.gpg; do : >"$scratch/etc/$empty"; done
printf '%s' "$preferences" >"$scratch/etc/preferences"
: >"$scratch/state/extended_states"
repo=$(uri_path "$bundle")
cat >"$scratch/etc/sources.list.d/debian.sources" <<SOURCES
Types: deb
URIs: file:$repo/debian/archive/debian/$snapshot
Suites: bookworm
Components: main
Architectures: amd64
Signed-By: $debian_key
Check-Valid-Until: no

Types: deb
URIs: file:$repo/debian/archive/debian-security/$snapshot
Suites: bookworm-security
Components: main
Architectures: amd64
Signed-By: $debian_key
Check-Valid-Until: no
SOURCES
if [[ -n $docker_key ]]; then
    cat >"$scratch/etc/sources.list.d/docker.sources" <<SOURCES
Types: deb
URIs: file:$repo/docker
Suites: bookworm
Components: stable
Architectures: amd64
Signed-By: $docker_key
SOURCES
fi
# Noclobber prevents replacing a file that appeared after preflight.
set -C
exec 3>"$config"
created_config=true
set +C
{
    printf 'Dir %s;\n' "$(quote_config "$scratch")"
    printf 'Dir::Etc %s;\n' "$(quote_config "$scratch/etc")"
    cat <<'CONFIG'
Dir::Etc::main "apt.conf";
Dir::Etc::parts "apt.conf.d";
Dir::Etc::sourcelist "sources.list";
Dir::Etc::sourceparts "sources.list.d";
Dir::Etc::netrc "auth.conf";
Dir::Etc::netrcparts "auth.conf.d";
Dir::Etc::trusted "trusted.gpg";
Dir::Etc::trustedparts "trusted.gpg.d";
Dir::Etc::preferences "preferences";
Dir::Etc::preferencesparts "preferences.d";
CONFIG
    printf 'Dir::State %s;\n' "$(quote_config "$scratch/state")"
    printf 'Dir::State::status %s;\n' "$(quote_config "$status")"
    cat <<'CONFIG'
Dir::State::lists "lists";
Dir::State::extended_states "extended_states";
CONFIG
    printf 'Dir::Cache %s;\n' "$(quote_config "$scratch/cache")"
    cat <<'CONFIG'
Dir::Cache::archives "archives";
Dir::Cache::pkgcache "";
Dir::Cache::srcpkgcache "";
CONFIG
    printf 'Dir::Log %s;\n' "$(quote_config "$scratch/log")"
    cat <<'CONFIG'
Dir::Log::Terminal "term.log";
Dir::Log::History "history.log";
Dir::Log::Planner "eipp.log.xz";
Acquire::Languages "none";
Acquire::PDiffs "false";
Acquire::Retries "0";
Acquire::AllowInsecureRepositories "false";
Acquire::AllowDowngradeToInsecureRepositories "false";
APT::Get::AllowUnauthenticated "false";
CONFIG
} >&3
exec 3>&-
chmod 644 -- "$config"
complete=true
printf '%s\n' "$config"
