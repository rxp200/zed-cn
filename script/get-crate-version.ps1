if ($args.Length -ne 1) {
    Write-Error "Usage: $($MyInvocation.MyCommand.Name) <crate_name>"
    exit 1
}

$crateName = $args[0]

$metadataJson = cargo metadata --no-deps --format-version=1
if ($LASTEXITCODE -ne 0) {
    throw "Failed to resolve Cargo metadata"
}

$metadata = $metadataJson | ConvertFrom-Json -AsHashtable
$package = $metadata["packages"] | Where-Object { $_["name"] -eq $crateName }
if ($package) {
    $package["version"]
}
else {
    Write-Error "Crate '$crateName' not found."
}
