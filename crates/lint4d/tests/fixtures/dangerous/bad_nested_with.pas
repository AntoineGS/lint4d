unit BadNestedWith;

interface

implementation

uses SysUtils;

procedure TestNestedWith;
var
  sl1, sl2: TStringList;
begin
  sl1 := TStringList.Create;
  sl2 := TStringList.Create;
  try
    with sl1 do
      with sl2 do
        Add('ambiguous');
  finally
    sl1.Free;
    sl2.Free;
  end;
end;

end.
