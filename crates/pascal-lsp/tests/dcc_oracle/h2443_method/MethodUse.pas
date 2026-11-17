unit MethodUse;
interface
implementation
uses MethodProv;
procedure P;
var O: TInlineClass; I: Integer;
begin
  O := TInlineClass.Create;
  I := O.GetValue;
  O.Free;
end;
end.
