#!/usr/bin/env bash
# The notes of a GitHub Release, on stdout: CHANGELOG.md's section for the
# version, then a short footer. Run by the Release workflow (the `meta` job).
#
#   .github/release-notes.sh VERSION [CHANGELOG.md]
#
# The section is the one whose heading names the version, however it is
# dressed: `## [0.2.0] — 2026-11-02`, `## 0.2.0`, `## v0.2.0 (2026-11-02)`.
# (`0.2.0` does not match `0.2.0-rc.1`: a pre-release has a section of its own.)
# With none, the `## [Unreleased]` section: what the first release of a
# project is made of, and what is next to be released. Its lead-in (the text
# before the first `###` subsection, usually about the file or the branch, not
# about the release) is left out. With neither, the notes are only the footer.
#
# Says what it used on stderr (a notice annotation under GitHub Actions; a
# warning when it had to fall back on [Unreleased]).
set -euo pipefail

version=${1:?usage: release-notes.sh VERSION [CHANGELOG.md]}
version=${version#v}
changelog=${2:-CHANGELOG.md}
repo=${GITHUB_REPOSITORY:-oceanwap/warden}
tag=v$version

note() { # level message
  if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::$1 title=Release notes::$2" >&2; else echo "release-notes: $2" >&2; fi
}

# The lines of the first `## ` section whose heading matches the pattern,
# without the heading and without blank lines around it.
section() { # ERE
  awk -v re="$1" '
    found && /^## / { exit }
    found { print; next }
    $0 ~ re { found = 1 }
  ' "$changelog" | awk '
    NF { for (i = 0; i < blank; i++) print ""; blank = 0; started = 1; print; next }
    started { blank++ }
  '
}

body=""
if [ -f "$changelog" ]; then
  dots=${version//./\\.}
  body=$(section "^##[[:space:]]+\\[?v?${dots}([^0-9A-Za-z.-]|\$)")
  if [ -n "$body" ]; then
    note notice "$changelog: the section for $version"
  else
    body=$(section '^##[[:space:]]+\[?[Uu]nreleased')
    if [ -n "$body" ]; then
      # Drop the lead-in, if there are subsections.
      if grep -q '^### ' <<<"$body"; then body=$(sed -n '/^### /,$p' <<<"$body"); fi
      note warning "$changelog has no section for $version: using [Unreleased]. Rename its heading to [$version] before the next release"
    else
      note warning "$changelog has no section for $version and no [Unreleased]: the notes are only the footer"
    fi
  fi
  # Link reference definitions at the end of the file belong to no section.
  body=$(awk '
    { line[NR] = $0 }
    END {
      n = NR
      while (n > 0 && (line[n] ~ /^[[:space:]]*$/ || line[n] ~ /^\[[^]]+\]:/)) n--
      for (i = 1; i <= n; i++) print line[i]
    }
  ' <<<"$body")
else
  note warning "no $changelog: the notes are only the footer"
fi

if [ -n "$body" ]; then
  printf '%s\n\n' "$body"
fi
cat <<EOF
---

Install and upgrade: see [Install](https://github.com/$repo/blob/$tag/README.md#install) and [Linux packages](https://github.com/$repo/blob/$tag/docs/packages.md) (\`.deb\` and \`.rpm\`). Every file here is listed in \`SHA256SUMS\`: \`sha256sum -c SHA256SUMS --ignore-missing\` checks the ones you downloaded. [Full changelog](https://github.com/$repo/blob/$tag/CHANGELOG.md).
EOF
