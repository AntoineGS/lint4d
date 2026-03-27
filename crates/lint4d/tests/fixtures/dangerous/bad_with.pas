unit BadWith;

interface

implementation

uses SysUtils;

procedure TestWith;
var
  sl: TStringList;
begin
  sl := TStringList.Create;
  try
    with sl do
    begin
      Add('hello');
    end;
  finally
    sl.Free;
  end;
end;

end.
