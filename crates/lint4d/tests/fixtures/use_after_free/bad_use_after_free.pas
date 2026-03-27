unit bad_use_after_free;
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
  end;
  aObj.ClassName;
end;
end.
