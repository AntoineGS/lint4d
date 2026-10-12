unit good_nil_checks_after_freeandnil;
interface
implementation
uses SysUtils;
procedure Test;
var
  X: TObject;
begin
  X := TObject.Create;
  FreeAndNil(X);
  if Assigned(X) then
    X.ClassName;
  if X <> nil then
    X.ClassName;
  if (nil <> X) then
    X.ClassName;
  if not Assigned(X) then
    Exit;
  X.ClassName;
end;

procedure TestEqualNil;
var
  X: TObject;
begin
  X := TObject.Create;
  FreeAndNil(X);
  if X = nil then
    Exit;
  X.ClassName;
end;
end.
