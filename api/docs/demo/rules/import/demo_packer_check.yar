rule demo_packer_check
{
    meta:
        author = "Thorium Demo"
        description = "Flags the demo packer's section marker"
    strings:
        $section = "THORIUM-DEMO-PACKED"
    condition:
        $section
}
