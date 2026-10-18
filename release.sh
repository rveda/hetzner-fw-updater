#!/bin/bash
# Builds a release of hetzner-fw-updater and publishes its Docker image.
# Run without arguments for usage.
set -euo pipefail

usage() {
    cat >&2 <<EOF
Usage: $0 <registry>
  <registry>  registry host and optional path, e.g. registry.example.com:5000/tools

The image is pushed as <registry>/hetzner-fw-updater:<version> and :latest,
where <version> is the version in Cargo.toml, and the current commit is
tagged v<version> in git. The working tree must be clean. An existing image
or git tag for the version is not overwritten unless FORCE=1 is set.
EOF
    exit 1
}

[ $# -eq 1 ] || usage
registry="${1%/}"

cd "$(dirname "$0")"

# Let cargo resolve the package name and version, so workspace inheritance and
# other Cargo.toml syntax are handled correctly.
read -r name version < <(
    cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json, sys; p = json.load(sys.stdin)["packages"][0]; print(p["name"], p["version"])'
) || true
if [ -z "${name:-}" ] || [ -z "${version:-}" ]; then
    echo "could not read the package name and version from Cargo.toml" >&2
    exit 1
fi
image="$registry/$name"
tag="v$version"

echo "==> Releasing $name $version to $image"

# The image must be built from a committed state, so the git tag below
# identifies exactly the source it was built from. Untracked files count too,
# since the Docker build context includes them.
if [ -n "$(git status --porcelain)" ]; then
    echo "the working tree has uncommitted changes or untracked files; commit or stash them first" >&2
    git status --short >&2
    exit 1
fi

if [ "${FORCE:-}" != 1 ]; then
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
        echo "git tag $tag already exists; bump the version in Cargo.toml or set FORCE=1" >&2
        exit 1
    fi
    if docker buildx imagetools inspect "$image:$version" >/dev/null 2>&1; then
        echo "$image:$version already exists; bump the version in Cargo.toml or set FORCE=1" >&2
        exit 1
    fi
fi

echo "==> Running tests"
cargo test --release --locked

# The image compiles its own binary in the Dockerfile's build stage.
echo "==> Building Docker image"
docker build -t "$image:$version" -t "$image:latest" .

echo "==> Pushing $image:$version and $image:latest"
docker push "$image:$version"
docker push "$image:latest"

# Tagged only after the push, so a failed release leaves no tag behind.
echo "==> Tagging $(git rev-parse --short HEAD) as $tag"
if [ "${FORCE:-}" = 1 ]; then
    git tag -f -a "$tag" -m "Release $version"
else
    git tag -a "$tag" -m "Release $version"
fi

echo "==> Published $image:$version"
echo "Push the tag with: git push origin $tag"
