unit WithStatement;

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
      Add('world');
    end;
  finally
    sl.Free;
  end;
end;

end.
