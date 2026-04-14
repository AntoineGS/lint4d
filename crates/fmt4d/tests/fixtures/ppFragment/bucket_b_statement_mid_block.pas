unit T;

interface

implementation

procedure DoIt;
var
  Foo, Bar: integer;
  FWSDebugLogs: boolean;
begin
  Foo := 1;
  {$IFDEF DEBUG}FWSDebugLogs := True;{$ENDIF}
  Bar := 2;
end;

end.
