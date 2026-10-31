#!/usr/bin/env bash
#
# build-reference.sh - Build the original C novacom client as a test reference.
#
# Clones openwebos/novacom at the pinned commit into reference/novacom
# (gitignored) and builds it with the upstream makefile. No privileges, no
# system-wide install. Output binary: reference/novacom/build-novacom/novacom
#
# Usage:
#   scripts/build-reference.sh [--dryrun] [--verbose] [--clean] [--help]
#
# Options:
#   --dryrun    Print the actions without running them.
#   -v, --verbose
#               Print each step and key variables.
#   --clean     Remove reference/ before building.
#   -h, --help  Print this help and exit.
#
# Environment:
#   NO_COLOR    Disable colored output when set.

set -euo pipefail

readonly REPO_URL="https://github.com/openwebos/novacom.git"
readonly PINNED_COMMIT="d9d8b772b86231c6490ea8b1ff0e44ddaaf8df05"
readonly BUILD_VERSION="ref-d9d8b77"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly REF_DIR="${ROOT}/reference/novacom"

dryrun=0
verbose=0
clean=0

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
	C_RED=$'\033[31m' C_GRN=$'\033[32m' C_DIM=$'\033[2m' C_OFF=$'\033[0m'
else
	C_RED="" C_GRN="" C_DIM="" C_OFF=""
fi

log() { if ((verbose)); then printf '%s%s%s\n' "${C_DIM}" "$*" "${C_OFF}"; fi; }
die() {
	printf '%serror:%s %s\n' "${C_RED}" "${C_OFF}" "$*" >&2
	exit 1
}
run() {
	if ((dryrun)); then
		printf 'dryrun: %s\n' "$*"
	else
		log "+ $*"
		"$@"
	fi
}
usage() { sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while (($#)); do
	case "$1" in
	--dryrun) dryrun=1 ;;
	-v | --verbose) verbose=1 ;;
	--clean) clean=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	*) die "unknown option: $1" ;;
	esac
	shift
done

command -v git >/dev/null || die "git not found"
command -v make >/dev/null || die "make not found"
command -v cc >/dev/null || die "cc not found"

log "root=${ROOT} ref=${REF_DIR} commit=${PINNED_COMMIT}"

if ((clean)); then
	run rm -rf "${ROOT}/reference"
fi

if [[ ! -d "${REF_DIR}/.git" ]]; then
	run mkdir -p "${ROOT}/reference"
	run git clone --quiet "${REPO_URL}" "${REF_DIR}"
fi
run git -C "${REF_DIR}" checkout --quiet --detach "${PINNED_COMMIT}"

if ((! dryrun)); then
	head="$(git -C "${REF_DIR}" rev-parse HEAD)"
	[[ "${head}" == "${PINNED_COMMIT}" ]] || die "checkout is ${head}, expected ${PINNED_COMMIT}"
fi

# BUILDVERSION must come from the environment: a make command-line variable
# bypasses the makefile's string quoting and breaks the build.
run env BUILDVERSION="${BUILD_VERSION}" make -C "${REF_DIR}" --no-print-directory -s

if ((! dryrun)); then
	[[ -x "${REF_DIR}/build-novacom/novacom" ]] || die "build produced no binary"
	printf '%sbuilt%s %s\n' "${C_GRN}" "${C_OFF}" "${REF_DIR}/build-novacom/novacom"
fi
