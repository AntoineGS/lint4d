unit RecordUse;
interface
implementation
uses RecordProv;
procedure P;
var R: TInlineRecord; I: Integer;
begin
  I := R.GetValue;
end;
end.
