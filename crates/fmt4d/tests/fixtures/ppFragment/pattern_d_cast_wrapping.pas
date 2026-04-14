unit T;

interface

implementation

function GetIt(v: Variant): string;
begin
  Result := {$IFDEF WBB_ANSI}AnsiString(v){$ELSE}v{$ENDIF};
end;

end.
