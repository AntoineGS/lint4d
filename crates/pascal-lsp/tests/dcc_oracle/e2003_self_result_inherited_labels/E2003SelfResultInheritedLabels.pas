unit E2003SelfResultInheritedLabels;
interface
implementation
type
  TBase = class
    procedure Base;
  end;
  TChild = class(TBase)
    procedure Child;
    function Value: Integer;
  end;
procedure TBase.Base;
begin
end;
procedure TChild.Child;
label L;
begin
  inherited Base;
  if Self <> nil then goto L;
L:
  Writeln;
end;
function TChild.Value: Integer;
begin
  Result := 1;
end;
end.
