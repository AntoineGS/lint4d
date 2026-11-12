unit PrivateAncestor;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  private
    procedure Hidden; virtual;
  end;

implementation

procedure TBase.Hidden;
begin
end;

end.
