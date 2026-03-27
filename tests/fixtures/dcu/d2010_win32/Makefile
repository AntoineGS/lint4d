# D2010 fixture build — uses dcc32 directly (D2010 MSBuild doesn't support DCC_BuildAllUnits)
RADSTUDIO_DIR = C:\Program Files (x86)\Embarcadero\RAD Studio\7.0
DCC32 = $(RADSTUDIO_DIR)\bin\dcc32.exe
LIBDIR = $(RADSTUDIO_DIR)\lib
OUTDIR = Win32\Debug

build:
	if not exist "$(OUTDIR)" mkdir "$(OUTDIR)"
	"$(DCC32)" -$O- -$W+ -B -Q -DDEBUG -E"$(OUTDIR)" -N0"$(OUTDIR)" -U"$(LIBDIR)" -R"$(LIBDIR)" Lint4dFixtures.dpr

clean:
	if exist "$(OUTDIR)" rmdir /S /Q "$(OUTDIR)"
