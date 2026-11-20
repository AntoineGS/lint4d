unit RtlUse;
interface
implementation
uses AnsiStrings;
procedure P;
var I: Integer;
begin
  I := AnsiCompareStr('a', 'b');
end;
end.
