unit E2003WithStatement;
interface
implementation
type TRec = record Field: Integer; end;
procedure P;
var R: TRec;
begin
  with R do Field := 1;
  with R do Missing := 1;
end;
end.
