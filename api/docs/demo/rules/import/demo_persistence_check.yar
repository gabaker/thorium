rule demo_persistence_check
{
    meta:
        author = "Thorium Demo"
        description = "Flags the demo implant's run-key persistence marker"
    strings:
        $runkey = "Software\\Microsoft\\Windows\\CurrentVersion\\Run" ascii wide
        $marker = "THORIUM-DEMO-PERSIST"
    condition:
        all of them
}
