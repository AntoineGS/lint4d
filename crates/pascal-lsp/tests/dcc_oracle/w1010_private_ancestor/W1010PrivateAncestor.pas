unit W1010PrivateAncestor;

interface

uses
  PrivateAncestor;

type
  TChild = class(TBase)
  public
    procedure Hidden;
  end;

implementation

procedure TChild.Hidden;
begin
end;

end.
