# Regenerates the README screenshot from fake data. Needs vhs and a Nerd Font.
# `task screenshot` runs this, uploads the result, and links it in the README.
#
# The `clear` in the typed command wipes the shell prompt and the echoed command
# line, so the shot is just the dashboard. There is no `Output`: only the
# screenshot is wanted, not a recording — but Screenshot lands on the next
# frame, so the trailing Sleep is what makes vhs actually write the file.

Set Shell "bash"
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 16
Set Width 1300
Set Height 880
Set Padding 30
Set Theme "Catppuccin Mocha"

Hide
Type "PS1=''" Enter
Type "clear; ./target/debug/examples/demo" Enter
Sleep 2s
Show
Sleep 1s
Screenshot demo.png
Sleep 1s
