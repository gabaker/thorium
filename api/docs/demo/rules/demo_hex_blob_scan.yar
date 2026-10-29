rule demo_hex_blob_scan
{
    meta:
        author = "demolab"
        description = "Flags large hex blobs often used to stage payloads"
    strings:
        $hex = /[0-9a-fA-F]{64,}/
    condition:
        $hex
}
