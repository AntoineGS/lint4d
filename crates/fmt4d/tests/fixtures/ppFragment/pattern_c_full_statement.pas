unit T;

interface

implementation

procedure DoIt;
var
  x: integer;
begin
  x := 0;
  {$IFDEF DEBUG}x := 1;{$ENDIF}
end;

end.
