unit OverloadUse;
interface
implementation
uses OverloadProv;
procedure P;
var I: Integer;
begin
  I := InlineValue(1);
  I := InlineValue('x');
end;
end.
