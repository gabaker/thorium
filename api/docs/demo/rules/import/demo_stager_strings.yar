rule demo_stager_strings
{
    meta:
        author = "Thorium Demo"
        description = "Flags the demo stager's download-and-execute markers"
    strings:
        $url = "THORIUM-DEMO-STAGER-URL"
        $exec = "THORIUM-DEMO-EXEC"
    condition:
        any of them
}
