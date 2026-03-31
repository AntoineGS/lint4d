unit BadWith;

interface

implementation

uses SysUtils;

procedure TestWith;
var
  Sl: TStringList;
begin
  Sl := TStringList.Create;
  try
    with Sl do
    begin
      Add('hello');
    end;
  finally
    Sl.Free;
  end;
end;

end.
