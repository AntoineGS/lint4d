unit bad_free_in_comment;

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
    // Conn.Free; -- TODO: uncomment later
  end;
end;

end.
