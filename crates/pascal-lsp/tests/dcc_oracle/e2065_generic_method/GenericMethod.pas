unit GenericMethod;

interface

type
  TBox<T> = class
    procedure Put(const Value: T);
    procedure Missing;
  end;

implementation

procedure TBox<T>.Put(const Value: T);
begin
end;

end.
