rule demo_broad_watchlist
{
    meta:
        author = "demolab"
        description = "Corporate watchlist: matches any PE or ELF header"
    strings:
        $mz = { 4D 5A }
        $elf = { 7F 45 4C 46 }
    condition:
        $mz at 0 or $elf at 0
}
