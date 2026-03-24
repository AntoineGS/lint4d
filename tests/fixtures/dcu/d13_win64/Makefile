# Makefile for Lint4dFixtures - produces DCU test fixtures
# Usage: nmake build (from this directory)

!ifndef CONFIG
CONFIG = Debug
!endif

!ifndef PLATFORM
PLATFORM = Win64
!endif

!ifndef RADSTUDIO_VERSION
RADSTUDIO_VERSION = 37.0
!endif

!ifndef RADSTUDIO_FOLDER
RADSTUDIO_FOLDER = Studio
!endif

!ifndef RADSTUDIO_DIR
RADSTUDIO_DIR = C:\Program Files (x86)\Embarcadero\$(RADSTUDIO_FOLDER)\$(RADSTUDIO_VERSION)
!endif

PROJECT = Lint4dFixtures.dproj
RSVARS = $(RADSTUDIO_DIR)\bin\rsvars.bat

build:
	cmd /S /C ""$(RSVARS)" & msbuild.exe /t:clean;build /p:config=$(CONFIG);Platform=$(PLATFORM);DCC_BuildAllUnits=true "$(PROJECT)""

clean:
	cmd /S /C ""$(RSVARS)" & msbuild.exe /t:clean /p:config=$(CONFIG);Platform=$(PLATFORM) "$(PROJECT)""
