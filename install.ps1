# Installs toss on Windows, from PowerShell:
#   irm https://raw.githubusercontent.com/plamen-g/toss/master/install.ps1 | iex
# It downloads the newest release from GitHub and lets it install itself: into
# %LOCALAPPDATA%\Programs\toss, with a Start menu entry and starting at login, then toss opens.
# A download made here isn't marked as coming from the internet, so Windows doesn't ask about it.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # Windows PowerShell downloads crawl while drawing progress
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $exe = Join-Path $env:TEMP 'toss-setup.exe'
    Write-Host 'Downloading toss...'
    Invoke-WebRequest 'https://github.com/plamen-g/toss/releases/latest/download/toss-windows-x86_64.exe' -OutFile $exe -UseBasicParsing
    & $exe --install
    Write-Host 'toss is installed: find it in the Start menu and the tray.'
}
