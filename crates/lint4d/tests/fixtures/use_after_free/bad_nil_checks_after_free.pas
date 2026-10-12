unit bad_nil_checks_after_free;
interface
implementation
uses SysUtils;
procedure Dangling;
var
  X: TObject;
begin
  X := TObject.Create;
  X.Free;
  if Assigned(X) then
    Exit;
end;

procedure NilPathReachesUse;
var
  X: TObject;
begin
  X := TObject.Create;
  FreeAndNil(X);
  if Assigned(X) then
    Exit;
  X.ClassName;
end;

procedure MaybeDangling(B: Boolean);
var
  X: TObject;
begin
  X := TObject.Create;
  if B then
    X.Free
  else
    FreeAndNil(X);
  if X <> nil then
    Exit;
end;
end.
