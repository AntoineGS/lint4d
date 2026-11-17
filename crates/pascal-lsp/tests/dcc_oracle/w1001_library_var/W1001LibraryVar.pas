unit W1001LibraryVar;

interface

var
  LibraryVariable: Integer library;

procedure UseLibraryVariable;

implementation

procedure UseLibraryVariable;
var
  Value: Integer;
begin
  Value := LibraryVariable;
end;

end.
