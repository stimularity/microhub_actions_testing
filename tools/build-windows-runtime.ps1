# Build a self-contained, offline R + Python payload for MicroHub (Windows x64).
#
# Far simpler than the macOS equivalent: R for Windows derives R_HOME from the
# executable's location, so the tree is relocatable as-is. There is no
# install_name_tool/codesign step and no fixed install path.
#
# Usage: pwsh tools/build-windows-runtime.ps1 -Staging <dir> -Output <file>

[CmdletBinding()]
param(
  [string]$Staging = "$PWD\build\windows-runtime",
  [string]$Output = "$PWD\payload.tar.gz",
  [switch]$SkipPackages,
  [switch]$SkipPython
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$PythonVersion = '3.11.9'
$PythonRelease = '20240415'

function Write-Step($message) {
  Write-Host ""
  Write-Host "=== $message"
}

# ---------------------------------------------------------------------------
# 1. R packages, installed INTO the R tree.
#
# This is the lesson from the macOS build: CI points R_LIBS_USER at a temp
# directory outside R, and packages installed there are silently not shipped.
# Everything must land in the tree that gets archived.
# ---------------------------------------------------------------------------
function Install-RPackages($rHome) {
  Write-Step "installing R packages into $rHome\library"

  $env:R_LIBS_USER = "$rHome\library"
  $env:R_LIBS_SITE = "$rHome\library"

  $rscript = "$rHome\bin\x64\Rscript.exe"

  & $rscript -e @"
options(repos = c(CRAN = 'https://cloud.r-project.org'), timeout = 1200)
install.packages('pak')
"@
  if ($LASTEXITCODE -ne 0) { throw "pak install failed" }

  & $rscript -e @"
options(repos = c(CRAN = 'https://cloud.r-project.org'), timeout = 1200)
pak::pkg_install(c(
  'dplyr', 'readr', 'lubridate', 'tidyr', 'purrr', 'forcats', 'tibble',
  'stringr', 'ggplot2', 'cowplot', 'scales', 'gridExtra', 'ggtext',
  'shiny', 'shinyjs', 'bslib', 'DT', 'markdown', 'sn',
  'mgcv', 'gam', 'MMWRweek', 'lightgbm', 'slider', 'scoringutils',
  'later',
  'cmu-delphi/epiprocess@main',
  'reichlab/simplets'
), ask = FALSE, upgrade = FALSE)
"@
  if ($LASTEXITCODE -ne 0) { throw "package install failed" }

  & $rscript -e @"
options(timeout = 1200)
install.packages('fmesher',
  repos = c(inlabruorg = 'https://inlabru-org.r-universe.dev', CRAN = 'https://cloud.r-project.org'),
  dependencies = c('Depends', 'Imports', 'LinkingTo'))
"@
  if ($LASTEXITCODE -ne 0) { throw "fmesher install failed" }

  & $rscript -e @"
options(timeout = 1800)
install.packages('INLA',
  repos = c(INLA = 'https://inla.r-inla-download.org/R/stable', CRAN = 'https://cloud.r-project.org'),
  dependencies = c('Depends', 'Imports', 'LinkingTo'))
library(INLA)
stopifnot(packageVersion('fmesher') >= '0.5.0')
"@
  if ($LASTEXITCODE -ne 0) { throw "INLA install failed" }
}

# ---------------------------------------------------------------------------
# 2. Standalone Python + torch. Note the flat layout on Windows:
#    python\python.exe, not python/bin/python3 as on macOS.
# ---------------------------------------------------------------------------
function Install-Python($pythonDir) {
  Write-Step "installing standalone Python $PythonVersion and torch"

  $url = "https://github.com/astral-sh/python-build-standalone/releases/download/$PythonRelease/cpython-$PythonVersion+$PythonRelease-x86_64-pc-windows-msvc-shared-install_only.tar.gz"
  $archive = Join-Path $env:TEMP 'python-standalone.tar.gz'

  Invoke-WebRequest -Uri $url -OutFile $archive
  if (Test-Path $pythonDir) { Remove-Item -Recurse -Force $pythonDir }

  # The archive contains a top-level python\ directory.
  & tar.exe -xzf $archive -C (Split-Path -Parent $pythonDir)
  if ($LASTEXITCODE -ne 0) { throw "python extraction failed" }

  $python = "$pythonDir\python.exe"
  & $python -m pip install --no-cache-dir --upgrade pip
  & $python -m pip install --no-cache-dir torch==2.2.2 pandas==2.2.3 numpy==1.26.4
  if ($LASTEXITCODE -ne 0) { throw "pip install failed" }
}

# ---------------------------------------------------------------------------
# 3. Verify against the staged tree only.
#
# The macOS check passed for two builds because it could still see the runner's
# own library. Clearing R_LIBS* is what makes this check mean anything.
# ---------------------------------------------------------------------------
function Test-Runtime($rHome, $pythonDir) {
  Write-Step "loading every package from the staged runtime"

  $env:R_LIBS = ''
  $env:R_LIBS_USER = 'nonexistent'
  $env:R_LIBS_SITE = ''

  & "$rHome\bin\x64\Rscript.exe" -e @"
pkgs <- c('shiny', 'later', 'dplyr', 'ggplot2', 'DT', 'bslib', 'shinyjs',
          'mgcv', 'gam', 'lightgbm', 'slider', 'scoringutils', 'MMWRweek',
          'epiprocess', 'simplets', 'fmesher', 'INLA')
for (p in pkgs) {
  suppressPackageStartupMessages(library(p, character.only = TRUE))
  cat('ok  ', p, ' ', as.character(packageVersion(p)), '\n', sep = '')
}
cat('R_HOME: ', R.home(), '\n', sep = '')
cat('libPaths:\n'); print(.libPaths())
"@
  if ($LASTEXITCODE -ne 0) { throw "R verification failed" }

  & "$pythonDir\python.exe" -c "import torch, pandas, numpy; print('ok   torch', torch.__version__, 'pandas', pandas.__version__, 'numpy', numpy.__version__)"
  if ($LASTEXITCODE -ne 0) { throw "python verification failed" }
}

# ---------------------------------------------------------------------------
# 4. Stage the Shiny app beside the runtime and archive the lot.
# ---------------------------------------------------------------------------
function Add-ShinyApp($stagingDir) {
  Write-Step "staging the Shiny app"

  $appDir = Join-Path $stagingDir 'shinyapp'
  New-Item -ItemType Directory -Force -Path $appDir | Out-Null

  Copy-Item 'app.R' -Destination $appDir
  foreach ($dir in @('R', 'ui', 'server', 'www', 'data')) {
    Copy-Item $dir -Destination $appDir -Recurse -Force
  }
}

# ---------------------------------------------------------------------------

$rHome = Join-Path $Staging 'R'
$pythonDir = Join-Path $Staging 'python'

New-Item -ItemType Directory -Force -Path $Staging | Out-Null

if (-not $SkipPackages) {
  Write-Step "copying the installed R into $rHome"
  $installed = (Get-Command R.exe -ErrorAction SilentlyContinue).Source
  if (-not $installed) { throw "R.exe not found on PATH" }
  # ...\R\bin\x64\R.exe -> ...\R
  $installedHome = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $installed))

  if (Test-Path $rHome) { Remove-Item -Recurse -Force $rHome }
  Copy-Item $installedHome -Destination $rHome -Recurse -Force

  Install-RPackages $rHome
}

if (-not $SkipPython) {
  Install-Python $pythonDir
}

Add-ShinyApp $Staging
Test-Runtime $rHome $pythonDir

Write-Step "archiving to $Output"
& tar.exe -czf $Output -C $Staging 'R' 'python' 'shinyapp'
if ($LASTEXITCODE -ne 0) { throw "archive failed" }

$size = (Get-Item $Output).Length
Write-Host "payload: $([math]::Round($size / 1MB)) MB"
