unit bad_pass_freed_as_param;
interface
implementation
procedure DoSomething(Obj: TObject);
begin
end;
procedure Test;
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  aObj.Free;
  DoSomething(aObj);
end;
end.
