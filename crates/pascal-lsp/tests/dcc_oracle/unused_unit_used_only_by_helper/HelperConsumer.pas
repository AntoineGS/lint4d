unit HelperConsumer;
interface
uses HelperBase, HelperUnit;
implementation
procedure P;
var
  Value: TBase;
begin
  Value := TBase.Create;
  try
    Value.Touch;
  finally
    Value.Free;
  end;
end;
end.
