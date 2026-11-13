unit E2003ScopedEnum;
interface
implementation
{$SCOPEDENUMS ON}
type TColor = (Red);
procedure P;
var C: TColor;
begin
  C := TColor.Red;
  C := TColor.Blue;
end;
end.
