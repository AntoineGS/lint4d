unit bad_fvar_standalone;

interface

implementation

procedure DoWork;
var
  FConnection: TFDConnection;
begin
  FConnection := TFDConnection.Create;
  FConnection.Execute('SELECT 1');
end;

end.
