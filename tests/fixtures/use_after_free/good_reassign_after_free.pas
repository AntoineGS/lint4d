unit good_reassign_after_free;
interface
implementation
procedure Test;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  aObj.Free;
  aObj := TObject.Create;
  aObj.ClassName;
  aObj.Free;
end;
end.
