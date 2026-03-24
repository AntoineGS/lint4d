unit bad_free_in_string;

interface

implementation

procedure DoWork;
var
  Conn: TConnection;
begin
  Conn := TConnection.Create;
  try
    Conn.Execute('SELECT 1');
  finally
    WriteLn('Remember to call Conn.Free');
  end;
end;

end.
