# Repository-local cargo wrapper.
#
#   .\scripts\cargo.ps1 build --release
#   .\scripts\cargo.ps1 run -- --help
#
# Equivalent to `. .\scripts\env.ps1; cargo @args`.
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

. "$PSScriptRoot\env.ps1"
& cargo @CargoArgs
exit $LASTEXITCODE
