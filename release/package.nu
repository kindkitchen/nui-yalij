use common.nu [checked verify-tag root]

# Build and smoke-test a native target, then archive the documented bin layout.
def main [target: string, tag: string] {
    cd $root
    let package = verify-tag $tag
    checked cargo build --locked --release --target $target
    let binary = $root | path join target $target release nui-yalij
    if (checked $binary --version) != $"nui-yalij ($package.version)" {
        error make --unspanned {msg: 'built executable version does not match the release'}
    }
    checked $binary --help | ignore
    let out = $root | path join target release-dist
    let stage = $out | path join $"stage-($target)"
    if ($stage | path exists) { rm --recursive --force $stage }
    mkdir $stage
    cp $binary LICENSE README.md $stage
    cp --recursive plugins $stage
    let archive = $out | path join $"($package.name)-($package.version)-($target).tar.gz"
    checked tar -czf $archive -C $stage nui-yalij LICENSE README.md plugins
    print $archive
}
