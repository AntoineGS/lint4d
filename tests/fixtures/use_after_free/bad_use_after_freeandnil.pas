unit bad_use_after_freeandnil;
interface
implementation
uses SysUtils;
procedure Test;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
  finally
    FreeAndNil(aObj);
  end;
  aObj.ClassName;
end;
end.
