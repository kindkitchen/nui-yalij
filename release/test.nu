use common.nu [checked bumped root]

# Exercise the real just gateway against disposable local Git repositories.
# No network, real remote, or working-tree version is touched.
def expect [condition: bool, message: string] {
    if not $condition { error make --unspanned {msg: $message} }
}

def main [] {
    cd $root
    expect ((bumped '1.2.3' patch) == '1.2.4') 'patch bump'
    expect ((bumped '1.2.3' minor) == '1.3.0') 'minor bump'
    expect ((bumped '0.9.9' major) == '1.0.0') 'major bump'
    expect ((try { bumped '1.2.3-beta' patch; false } catch { true })) 'reject prerelease versions'
    expect ((try { bumped '1.2.3' path; false } catch { true })) 'reject unknown bump'
    let fixture = mktemp -d | str trim
    try {
        let repo = $fixture | path join repo
        let remote = $fixture | path join origin.git
        mkdir ($repo | path join release)
        cp release/common.nu release/version.nu ($repo | path join release)
        cp justfile $repo
        '[package]
name = "nui-yalij"
version = "0.1.0"
' | save ($repo | path join Cargo.toml)
        '[[package]]
name = "other"
version = "0.1.0"

[[package]]
name = "nui-yalij"
version = "0.1.0"
' | save ($repo | path join Cargo.lock)
        'if ($env.FAIL_CHECK? | default "0") == "1" { error make {msg: "fixture validation failure"} }
' | save ($repo | path join release/check.nu)
        checked git init --bare $remote | ignore
        checked git -C $repo init --initial-branch nui-yalij/main | ignore
        checked git -C $repo config user.name 'release test'
        checked git -C $repo config user.email 'test@example.invalid'
        checked git -C $repo add .
        checked git -C $repo commit -m initial | ignore
        checked git -C $repo remote add origin $remote
        checked git -C $repo push --set-upstream origin nui-yalij/main | ignore
        let gateway = $repo | path join justfile
        let initial = checked git -C $repo rev-parse HEAD
        checked just --justfile $gateway version patch | ignore
        expect ((checked git -C $repo rev-parse HEAD) == $initial) 'preview must not commit'
        expect ((open ($repo | path join Cargo.toml) | get package.version) == '0.1.0') 'preview must not bump'
        expect ((checked git -C $repo tag --list | is-empty)) 'preview must not tag'
        checked just --justfile $gateway version minor --apply | ignore
        expect ((open ($repo | path join Cargo.toml) | get package.version) == '0.2.0') 'manifest bump'
        let lock = open --raw ($repo | path join Cargo.lock)
        expect ($lock | str contains "name = \"other\"\nversion = \"0.1.0\"") 'leave dependencies locked'
        expect ($lock | str contains "name = \"nui-yalij\"\nversion = \"0.2.0\"") 'lockfile bump'
        expect ((checked git -C $repo tag --list) == 'nui-yalij-0.2.0') 'matching tag'
        expect ((checked git -C $repo status --porcelain | is-empty)) 'apply leaves a clean tree'
        let unsynced = just --justfile $gateway version patch | complete
        expect ($unsynced.exit_code != 0) 'block unsynchronized branch'
        checked git -C $repo push origin nui-yalij/main | ignore
        let failure = with-env {FAIL_CHECK: '1'} { just --justfile $gateway version patch --apply | complete }
        expect ($failure.exit_code != 0) 'validation must fail'
        expect ((open ($repo | path join Cargo.toml) | get package.version) == '0.2.0') 'restore failed bump'
        expect ((checked git -C $repo status --porcelain | is-empty)) 'failed validation leaves a clean tree'
        'uncommitted' | save ($repo | path join dirty)
        expect ((just --justfile $gateway version patch --apply | complete).exit_code != 0) 'block dirty checkout'
        rm ($repo | path join dirty)
        checked git --git-dir $remote tag nui-yalij-0.2.1 nui-yalij/main
        expect ((just --justfile $gateway version patch --apply | complete).exit_code != 0) 'block existing remote tag'
    } catch {|error|
        rm --recursive --force $fixture
        error make --unspanned {msg: $error.msg}
    }
    rm --recursive --force $fixture
    print 'Release helper tests passed.'
}
