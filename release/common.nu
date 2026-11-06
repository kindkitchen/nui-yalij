# Shared release checks: every failed external command is a hard error.
export const root = path self ..

export def --wrapped checked [...command: string] {
    let result = run-external ...$command | complete
    if $result.exit_code != 0 {
        error make --unspanned {msg: $"failed: ($command | str join ' ')\n($result.stdout)($result.stderr)"}
    }
    if ($result.stderr | is-not-empty) { print -en $result.stderr }
    $result.stdout | str trim --right
}

export def bumped [version: string, kind: string]: nothing -> string {
    let parsed = $version | parse -r '^(?<major>0|[1-9][0-9]*)\.(?<minor>0|[1-9][0-9]*)\.(?<patch>0|[1-9][0-9]*)$'
    if ($parsed | is-empty) { error make --unspanned {msg: $"expected a stable SemVer version, got ($version)"} }
    let current = $parsed | first | update major { into int } | update minor { into int } | update patch { into int }
    let parts = match $kind {
        patch => [$current.major $current.minor ($current.patch + 1)]
        minor => [$current.major ($current.minor + 1) 0]
        major => [($current.major + 1) 0 0]
        _ => { error make --unspanned {msg: "choose patch, minor, or major"} }
    }
    $parts | each { into string } | str join '.'
}

export def release-tag [version: string]: nothing -> string { $"nui-yalij-($version)" }

export def verify-tag [tag: string] {
    let package = open Cargo.toml | get package
    let expected = release-tag $package.version
    if $tag != $expected { error make --unspanned {msg: $"tag ($tag) does not match ($expected)"} }
    $package
}
