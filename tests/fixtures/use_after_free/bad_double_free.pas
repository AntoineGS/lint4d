unit bad_double_free;
interface
implementation
procedure Test;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
  finally
    aObj.Free;
    aObj.Free;
  end;
end;
end.
