unit good_no_use_after_free;
interface
implementation
procedure Test;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
    aObj.ClassName;
  finally
    aObj.Free;
  end;
end;
end.
