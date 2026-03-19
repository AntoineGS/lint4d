unit Generics;

interface

type
  TGenericList<T> = class
  private
    FItems: array of T;
  public
    procedure Add(const Item: T);
    function Get(Index: Integer): T;
  end;

  TConstrainedClass<T: class> = class
  public
    procedure Process(Item: T);
  end;

implementation

procedure TGenericList<T>.Add(const Item: T);
begin
  SetLength(FItems, Length(FItems) + 1);
  FItems[High(FItems)] := Item;
end;

function TGenericList<T>.Get(Index: Integer): T;
begin
  Result := FItems[Index];
end;

procedure TConstrainedClass<T>.Process(Item: T);
begin
end;

end.
