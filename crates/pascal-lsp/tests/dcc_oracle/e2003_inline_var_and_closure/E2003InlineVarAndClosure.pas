unit E2003InlineVarAndClosure;
interface
implementation
type TProc = reference to procedure;
procedure P;
begin
  var X := 1;
  var F: TProc := procedure
    begin
      Writeln(X);
      Writeln(MisspeltCaptured);
    end;
  F();
end;
end.
