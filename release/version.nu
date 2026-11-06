use common.nu [checked bumped release-tag root]

# Bump a release version, with no writes unless --apply is supplied.
#
# Requires a clean nui-yalij/main synchronized with origin. --apply updates
# Cargo.toml/Cargo.lock, validates, commits and tags locally. --release also
# atomically pushes branch/tag and publishes a GitHub Release, which triggers CI.
@example 'preview the next patch' { just version patch }
@example 'prepare a minor release locally' { just version minor --apply }
@example 'prepare and publish a major release' { just version major --apply --release }
def main [
    kind: string  # patch, minor, or major; reset subordinate components.
    --apply       # Validate, commit the version change and create its local tag.
    --release     # Also push and publish a GitHub Release; requires --apply.
] {
    cd $root
    if $release and not $apply { error make --unspanned {msg: "--release requires --apply"} }
    let manifest = open --raw Cargo.toml
    let lock = open --raw Cargo.lock
    let old = open Cargo.toml | get package.version
    let next = bumped $old $kind
    let tag = release-tag $next
    let branch = checked git branch --show-current
    if $branch != 'nui-yalij/main' { error make --unspanned {msg: "release from nui-yalij/main"} }
    if (checked git status --porcelain | is-not-empty) { error make --unspanned {msg: "commit or stash changes before preparing a version"} }
    let head = checked git rev-parse HEAD
    let remote = checked git ls-remote origin $"refs/heads/($branch)" $"refs/tags/($tag)" | lines | parse "{sha}\t{ref}"
    if ($remote | where ref == $"refs/tags/($tag)" | is-not-empty) { error make --unspanned {msg: $"remote tag ($tag) already exists"} }
    let remote_branch = $remote | where ref == $"refs/heads/($branch)"
    if ($remote_branch | is-empty) or ($remote_branch | first | get sha) != $head {
        error make --unspanned {msg: "synchronize the local branch with origin before preparing a version"}
    }
    if (git show-ref --verify --quiet $"refs/tags/($tag)" | complete).exit_code == 0 {
        error make --unspanned {msg: $"local tag ($tag) already exists"}
    }
    let from = $"name = \"nui-yalij\"\nversion = \"($old)\""
    if not ($lock | str contains $from) { error make --unspanned {msg: "Cargo.lock does not match the package version"} }
    if $release {
        let origin = checked git remote get-url origin
        if $origin not-in ['https://github.com/kindkitchen/nui-yalij.git' 'https://github.com/kindkitchen/nui-yalij' 'git@github.com:kindkitchen/nui-yalij.git'] {
            error make --unspanned {msg: 'release origin must be kindkitchen/nui-yalij'}
        }
        checked gh auth status | ignore
    }
    print $"($old) -> ($next): Cargo.toml, Cargo.lock; validate; commit; tag ($tag)."
    if $release { print 'Then atomically push branch/tag and publish the GitHub Release.' }
    if not $apply { print 'Observed only; add --apply to make these changes.'; return }

    $manifest | str replace --regex '(?m)^version = "[^"]+"' $"version = \"($next)\"" | save --force Cargo.toml
    $lock | str replace $from $"name = \"nui-yalij\"\nversion = \"($next)\"" | save --force Cargo.lock
    try {
        checked nu release/check.nu
    } catch {|error|
        # The preflight required a clean tree; restore only our two writes.
        $manifest | save --force Cargo.toml
        $lock | save --force Cargo.lock
        error make --unspanned {msg: $"Validation failed; version files restored.\n($error.msg)"}
    }
    checked git add -- Cargo.toml Cargo.lock
    checked git commit -m $"release ($tag)"
    checked git tag --annotate $tag --message $tag
    if $release {
        checked git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push --atomic origin $"HEAD:refs/heads/($branch)" $"refs/tags/($tag)"
        checked gh release create $tag --repo kindkitchen/nui-yalij --verify-tag --title $tag --generate-notes --latest
    } else {
        print $"Prepared locally. Publish ($tag) as a GitHub Release when ready; no remote writes made."
    }
}
