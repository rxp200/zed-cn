
function ParseZedWorkspace {
    $metadataJson = cargo metadata --no-deps --offline
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to resolve Cargo workspace metadata"
    }

    $metadata = $metadataJson | ConvertFrom-Json -AsHashtable
    $env:ZED_WORKSPACE = $metadata["workspace_root"]
    $env:RELEASE_VERSION = $metadata["packages"] |
        Where-Object { $_["name"] -eq "zed" } |
        ForEach-Object { $_["version"] }
}
