rule demo_keylogger_strings
{
    meta:
        author = "Thorium Demo"
        description = "Detects the demo keylogger's window-hook strings"
    strings:
        $hook = "SetWindowsHookExW" ascii wide
        $log = "THORIUM-DEMO-KEYLOG"
    condition:
        all of them
}
