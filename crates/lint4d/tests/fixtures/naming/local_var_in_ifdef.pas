unit LocalVarInIfdef;

interface

implementation

procedure Foo;
var
  {$IFDEF DEBUG}
  bad_var: Integer;
  {$ENDIF}
  GoodVar: Integer;
begin
end;

end.
